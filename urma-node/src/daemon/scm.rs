use super::{ServeOptions, inputs::invalid, serve};
use crate::{Error, config, platform::ServiceSettings, service::ShutdownHandle};
use std::{
    ffi::OsString,
    fs::{File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard, OnceLock,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    thread,
    time::Duration,
};
use windows_service::{
    define_windows_service,
    service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    },
    service_control_handler::{self, ServiceControlHandlerResult, ServiceStatusHandle},
    service_dispatcher,
};

struct Dispatch;

impl Dispatch {
    const NOT_FROM_SERVICE_MANAGER: i32 = 1063;
    const START_WAIT: Duration = Duration::from_secs(30);
    const START_TICK: Duration = Duration::from_secs(10);
    const SETTINGS_FAILED: u32 = 2;
    const SERVE_FAILED: u32 = 1;
    const LOG: &str = "service.log";
    const LOG_ROTATED: &str = "service.log.1";
    const LOG_CAP: u64 = 4 * 1024 * 1024;
}

static SETTINGS: OnceLock<PathBuf> = OnceLock::new();

define_windows_service!(ffi_service_main, service_main);

/// Under the service manager the dispatcher blocks until the service
/// stopped and service.json is read inside the service; from a console the
/// dispatcher refuses and this serves in the foreground with `signals`.
pub(super) fn run(
    settings: &Path,
    signals: impl FnOnce(ShutdownHandle) -> Result<(), Error>,
    log: &mut impl Write,
) -> Result<(), Error> {
    match SETTINGS.set(settings.to_path_buf()) {
        Ok(()) => {}
        Err(previous) => {
            return Err(Error::Busy(format!(
                "service settings already set to {}",
                previous.display()
            )));
        }
    }
    match service_dispatcher::start(config::SERVICE_NAME, ffi_service_main) {
        Ok(()) => Ok(()),
        Err(windows_service::Error::Winapi(cause)) => match cause.raw_os_error() {
            Some(Dispatch::NOT_FROM_SERVICE_MANAGER) => {
                let options = ServeOptions::from_settings(ServiceSettings::read(settings)?);
                serve(options, signals, log)
            }
            Some(code) => Err(invalid(format!("service dispatcher error {code}: {cause}"))),
            None => Err(invalid(cause)),
        },
        Err(cause) => Err(invalid(cause)),
    }
}

fn service_main(_arguments: Vec<OsString>) {
    match under_service_manager() {
        Ok(()) => {}
        Err(error) => tracing::error!(%error, "urma-node service stopped with an error"),
    }
}

#[derive(Default)]
struct Stop {
    requested: bool,
    handle: Option<ShutdownHandle>,
}

fn locked(stop: &Mutex<Stop>) -> MutexGuard<'_, Stop> {
    match stop.lock() {
        Ok(stop) => stop,
        Err(poisoned) => {
            tracing::error!(error = %poisoned, "service stop state poisoned; stopping anyway");
            poisoned.into_inner()
        }
    }
}

fn request(stop: &Mutex<Stop>) {
    let mut stop = locked(stop);
    stop.requested = true;
    for handle in stop.handle.iter() {
        handle.request();
    }
}

fn status(
    state: ServiceState,
    accepted: ServiceControlAccept,
    exit: u32,
    checkpoint: u32,
) -> ServiceStatus {
    ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: accepted,
        exit_code: match exit {
            0 => ServiceExitCode::Win32(0),
            code => ServiceExitCode::ServiceSpecific(code),
        },
        checkpoint,
        wait_hint: match state {
            ServiceState::StartPending => Dispatch::START_WAIT,
            _ => Duration::ZERO,
        },
        process_id: None,
    }
}

/// Keeps a pending start alive: the checkpoint advances every tick until the
/// control listener is bound or startup failed.
struct Starting {
    done: Arc<AtomicBool>,
    checkpoint: Arc<AtomicU32>,
}

impl Starting {
    fn begin(handle: ServiceStatusHandle) -> Result<Self, Error> {
        let starting = Self {
            done: Arc::new(AtomicBool::new(false)),
            checkpoint: Arc::new(AtomicU32::new(1)),
        };
        report(
            handle,
            ServiceState::StartPending,
            ServiceControlAccept::empty(),
            0,
            1,
        )?;
        let (done, checkpoint) = (Arc::clone(&starting.done), Arc::clone(&starting.checkpoint));
        thread::spawn(move || {
            loop {
                thread::sleep(Dispatch::START_TICK);
                if done.load(Ordering::Acquire) {
                    break;
                }
                let next = checkpoint.fetch_add(1, Ordering::AcqRel) + 1;
                let pending = status(
                    ServiceState::StartPending,
                    ServiceControlAccept::empty(),
                    0,
                    next,
                );
                match handle.set_service_status(pending) {
                    Ok(()) => {}
                    Err(error) => tracing::warn!(%error, "start checkpoint not reported"),
                }
            }
        });
        Ok(starting)
    }

    fn finish(&self) {
        self.done.store(true, Ordering::Release);
    }
}

fn under_service_manager() -> Result<(), Error> {
    let stop = Arc::new(Mutex::new(Stop::default()));
    let control = Arc::clone(&stop);
    let handle =
        service_control_handler::register(config::SERVICE_NAME, move |event| match event {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                request(&control);
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        })
        .map_err(invalid)?;
    let starting = Starting::begin(handle)?;
    let options = match settings() {
        Ok(options) => options,
        Err(error) => {
            starting.finish();
            stopped(handle, Dispatch::SETTINGS_FAILED)?;
            return Err(error);
        }
    };
    let mut log = open_log(&options.data)?;
    install_tracing(&options.data)?;
    let outcome = serve(
        options,
        |shutdown| {
            starting.finish();
            bound(handle, &stop, shutdown)
        },
        &mut log,
    );
    starting.finish();
    let exit = match &outcome {
        Ok(()) => 0,
        Err(error) => {
            writeln!(log, "urma-node: {error}")?;
            tracing::error!(%error, "urma-node service failed");
            Dispatch::SERVE_FAILED
        }
    };
    stopped(handle, exit)?;
    outcome
}

fn settings() -> Result<ServeOptions, Error> {
    let path = SETTINGS
        .get()
        .ok_or(Error::Invalid("service settings path missing".into()))?;
    Ok(ServeOptions::from_settings(ServiceSettings::read(path)?))
}

/// The service can write only below its data directory; the log is capped by
/// keeping one rotated predecessor.
fn open_log(data: &Path) -> Result<File, Error> {
    let path = data.join(Dispatch::LOG);
    match std::fs::metadata(&path) {
        Ok(metadata) if metadata.len() > Dispatch::LOG_CAP => {
            std::fs::rename(&path, data.join(Dispatch::LOG_ROTATED))?;
        }
        Ok(_) => {}
        Err(cause) if cause.kind() == io::ErrorKind::NotFound => {}
        Err(cause) => return Err(cause.into()),
    }
    Ok(OpenOptions::new().create(true).append(true).open(path)?)
}

fn install_tracing(data: &Path) -> Result<(), Error> {
    let writer = Mutex::new(open_log(data)?);
    tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        .try_init()
        .map_err(invalid)
}

fn bound(
    handle: ServiceStatusHandle,
    stop: &Mutex<Stop>,
    shutdown: ShutdownHandle,
) -> Result<(), Error> {
    {
        let mut stop = locked(stop);
        if stop.requested {
            shutdown.request();
        }
        stop.handle = Some(shutdown);
    }
    let accepted = ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN;
    report(handle, ServiceState::Running, accepted, 0, 0)
}

fn stopped(handle: ServiceStatusHandle, exit: u32) -> Result<(), Error> {
    report(
        handle,
        ServiceState::Stopped,
        ServiceControlAccept::empty(),
        exit,
        0,
    )
}

fn report(
    handle: ServiceStatusHandle,
    state: ServiceState,
    accepted: ServiceControlAccept,
    exit: u32,
    checkpoint: u32,
) -> Result<(), Error> {
    handle
        .set_service_status(status(state, accepted, exit, checkpoint))
        .map_err(invalid)
}
