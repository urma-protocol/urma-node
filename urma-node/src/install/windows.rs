use crate::{
    config::SERVICE_NAME,
    install::{
        Error, Init, Registration, Runner, Status, System,
        windows_acl::{administered, reclaim, trusted_binary},
    },
    platform::ensure_defaults,
};
use std::{
    ffi::{OsStr, OsString},
    fs,
    io::Write,
    thread,
    time::Duration,
};
use windows_service::{
    Error as ServiceError,
    service::{
        Service, ServiceAccess, ServiceAction, ServiceActionType, ServiceErrorControl,
        ServiceFailureActions, ServiceFailureResetPeriod, ServiceInfo, ServiceStartType,
        ServiceState, ServiceType,
    },
    service_manager::{ServiceManager, ServiceManagerAccess},
};

pub struct Scm;

impl Scm {
    const NAME: &'static str = SERVICE_NAME;
    const DISPLAY: &'static str = "URMA Node";
    const DOES_NOT_EXIST: i32 = 1060;
    const STOP_POLLS: u32 = 60;
    const POLL: Duration = Duration::from_millis(500);
    const RESTART_DELAY: Duration = Duration::from_secs(10);
    const RESET_PERIOD: Duration = Duration::from_secs(86_400);

    fn access() -> ServiceAccess {
        ServiceAccess::QUERY_STATUS
            | ServiceAccess::QUERY_CONFIG
            | ServiceAccess::CHANGE_CONFIG
            | ServiceAccess::START
            | ServiceAccess::STOP
            | ServiceAccess::DELETE
    }

    fn manager() -> Result<ServiceManager, Error> {
        Ok(ServiceManager::local_computer(
            None::<&str>,
            ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
        )
        .map_err(scm("OpenSCManager"))?)
    }

    fn open(manager: &ServiceManager) -> Result<Vec<Service>, Error> {
        match manager.open_service(Self::NAME, Self::access()) {
            Ok(service) => Ok(vec![service]),
            Err(ServiceError::Winapi(error)) => match error.raw_os_error() {
                Some(code) if code == Self::DOES_NOT_EXIST => Ok(Vec::new()),
                Some(..) | None => Err(scm("OpenService")(ServiceError::Winapi(error))),
            },
            Err(error) => Err(scm("OpenService")(error)),
        }
    }

    fn info(binary: &std::path::Path) -> ServiceInfo {
        ServiceInfo {
            name: OsString::from(Self::NAME),
            display_name: OsString::from(Self::DISPLAY),
            service_type: ServiceType::OWN_PROCESS,
            start_type: ServiceStartType::AutoStart,
            error_control: ServiceErrorControl::Normal,
            executable_path: binary.to_path_buf(),
            launch_arguments: vec![OsString::from("service"), OsString::from("run")],
            dependencies: vec![],
            account_name: Some(OsString::from(format!(r"NT SERVICE\{}", Self::NAME))),
            account_password: None,
        }
    }

    fn recover(service: &Service) -> Result<(), Error> {
        let restart = ServiceAction {
            action_type: ServiceActionType::Restart,
            delay: Self::RESTART_DELAY,
        };
        service
            .update_failure_actions(ServiceFailureActions {
                reset_period: ServiceFailureResetPeriod::After(Self::RESET_PERIOD),
                reboot_msg: None,
                command: None,
                actions: Some(vec![restart.clone(), restart.clone(), restart]),
            })
            .map_err(scm("ChangeServiceConfig2 failure actions"))?;
        service
            .set_failure_actions_on_non_crash_failures(true)
            .map_err(scm("ChangeServiceConfig2 failure flag"))?;
        Ok(())
    }

    fn start(service: &Service) -> Result<(), Error> {
        service.start::<&OsStr>(&[]).map_err(scm("StartService"))?;
        Ok(())
    }

    fn stop(service: &Service) -> Result<(), Error> {
        if service
            .query_status()
            .map_err(scm("QueryServiceStatus"))?
            .current_state
            == ServiceState::Stopped
        {
            return Ok(());
        }
        service.stop().map_err(scm("ControlService stop"))?;
        for _ in 0..Self::STOP_POLLS {
            if service
                .query_status()
                .map_err(scm("QueryServiceStatus"))?
                .current_state
                == ServiceState::Stopped
            {
                return Ok(());
            }
            thread::sleep(Self::POLL);
        }
        Err(Error::Failed(format!("{} did not stop", Self::NAME)))
    }
}

fn scm(call: &'static str) -> impl FnOnce(ServiceError) -> Error {
    move |error| Error::Windows { call, error }
}

fn native(init: Init) -> Result<(), Error> {
    match init {
        Init::Windows => Ok(()),
        Init::Systemd | Init::Openrc | Init::Runit => Err(Error::Failed(format!(
            "{init:?} is not available on Windows"
        ))),
    }
}

impl<R: Runner> System<R> {
    pub fn registration(&self) -> Result<Registration, Error> {
        match Scm::open(&Scm::manager()?)?.is_empty() {
            true => Ok(Registration::Absent),
            false => Ok(Registration::Registered(Init::Windows)),
        }
    }

    pub fn install(&mut self, init: Init, out: &mut impl Write) -> Result<(), Error> {
        native(init)?;
        trusted_binary(&self.host.binary)?;
        let manager = Scm::manager()?;
        let info = Scm::info(&self.host.binary);
        let existing = Scm::open(&manager)?;
        for service in &existing {
            Scm::stop(service)?;
        }
        let paths = self.paths();
        reclaim(&paths)?;
        let service = match existing.into_iter().next() {
            Some(service) => {
                service
                    .change_config(&info)
                    .map_err(scm("ChangeServiceConfig"))?;
                service
            }
            None => manager
                .create_service(&info, Scm::access())
                .map_err(scm("CreateService"))?,
        };
        Scm::recover(&service)?;
        ensure_defaults(&paths)?;
        administered(&paths.data)?;
        Scm::start(&service)?;
        writeln!(out, "{} is installed and running.", Scm::DISPLAY)?;
        Ok(())
    }

    pub fn update(&mut self, init: Init) -> Result<(), Error> {
        native(init)?;
        trusted_binary(&self.host.binary)?;
        let manager = Scm::manager()?;
        let Some(service) = Scm::open(&manager)?.into_iter().next() else {
            return Err(Error::NotInstalled);
        };
        service
            .change_config(&Scm::info(&self.host.binary))
            .map_err(scm("ChangeServiceConfig"))?;
        Scm::recover(&service)?;
        Scm::stop(&service)?;
        Scm::start(&service)
    }

    pub fn uninstall(&mut self, purge: bool) -> Result<(), Error> {
        let manager = Scm::manager()?;
        for service in Scm::open(&manager)? {
            Scm::stop(&service)?;
            service.delete().map_err(scm("DeleteService"))?;
        }
        let paths = self.paths();
        if purge && paths.state_dir.is_dir() {
            fs::remove_dir_all(&paths.state_dir)?;
        }
        Ok(())
    }

    pub fn state(&mut self, init: Init) -> Result<Status, Error> {
        let manager = Scm::manager()?;
        let mut status = Status {
            init,
            registered: false,
            enabled: false,
            running: false,
            listening: false,
        };
        for service in Scm::open(&manager)? {
            status.registered = true;
            status.enabled = service
                .query_config()
                .map_err(scm("QueryServiceConfig"))?
                .start_type
                == ServiceStartType::AutoStart;
            status.running = service
                .query_status()
                .map_err(scm("QueryServiceStatus"))?
                .current_state
                == ServiceState::Running;
        }
        Ok(status)
    }
}
