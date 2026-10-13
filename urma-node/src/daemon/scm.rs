use super::{ServeOptions, inputs::invalid, serve};
use crate::{Error, config, service::ShutdownHandle};
use std::{
    ffi::OsString,
    io::{self, Write},
    sync::{Arc, Mutex, MutexGuard, OnceLock},
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
    const START_WAIT: Duration = Duration::from_secs(120);
    const FAILED: u32 = 1;
}

static OPTIONS: OnceLock<ServeOptions> = OnceLock::new();

define_windows_service!(ffi_service_main, service_main);

/// Under the service manager the dispatcher blocks until the service
/// stopped; from a console the dispatcher refuses and this runs in the
/// foreground with the caller's signals.
pub(super) fn run(
    options: ServeOptions,
    signals: impl FnOnce(ShutdownHandle) -> Result<(), Error>,
    log: &mut impl Write,
) -> Result<(), Error> {
    match OPTIONS.set(options.clone()) {
        Ok(()) => {}
        Err(previous) => {
            return Err(Error::Busy(format!(
                "service options already set for {}",
                previous.data.display()
            )));
        }
    }
    match service_dispatcher::start(config::SERVICE_NAME, ffi_service_main) {
        Ok(()) => Ok(()),
        Err(windows_service::Error::Winapi(cause)) => match cause.raw_os_error() {
            Some(Dispatch::NOT_FROM_SERVICE_MANAGER) => serve(options, signals, log),
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

fn status(state: ServiceState, accepted: ServiceControlAccept, exit: u32) -> ServiceStatus {
    ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: accepted,
        exit_code: match exit {
            0 => ServiceExitCode::Win32(0),
            code => ServiceExitCode::ServiceSpecific(code),
        },
        checkpoint: 0,
        wait_hint: match state {
            ServiceState::StartPending => Dispatch::START_WAIT,
            _ => Duration::ZERO,
        },
        process_id: None,
    }
}

fn under_service_manager() -> Result<(), Error> {
    let options = OPTIONS
        .get()
        .cloned()
        .ok_or(Error::Invalid("service options missing".into()))?;
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
    report(
        handle,
        ServiceState::StartPending,
        ServiceControlAccept::empty(),
        0,
    )?;
    let outcome = serve(
        options,
        |shutdown| bound(handle, &stop, shutdown),
        &mut io::sink(),
    );
    let exit = match &outcome {
        Ok(()) => 0,
        Err(error) => {
            tracing::error!(%error, "urma-node service failed");
            Dispatch::FAILED
        }
    };
    report(
        handle,
        ServiceState::Stopped,
        ServiceControlAccept::empty(),
        exit,
    )?;
    outcome
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
    report(handle, ServiceState::Running, accepted, 0)
}

fn report(
    handle: ServiceStatusHandle,
    state: ServiceState,
    accepted: ServiceControlAccept,
    exit: u32,
) -> Result<(), Error> {
    handle
        .set_service_status(status(state, accepted, exit))
        .map_err(invalid)
}
