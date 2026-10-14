#[cfg(unix)]
use crate::install::Outcome;
use crate::{
    install::{Action, Commands, Error, Host, InitChoice, Runner},
    platform::Paths,
};
use serde::Serialize;
use std::{
    io::Write,
    path::{Component, Path, PathBuf},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Init {
    Systemd,
    Openrc,
    Runit,
    Windows,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Registration {
    Registered(Init),
    Absent,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Status {
    pub init: Init,
    pub registered: bool,
    pub enabled: bool,
    pub running: bool,
}

pub struct Search;

impl Search {
    pub const DIRS: [&'static str; 4] = ["/usr/sbin", "/usr/bin", "/sbin", "/bin"];
}

pub struct System<R: Runner> {
    pub root: PathBuf,
    pub host: Host,
    pub runner: R,
}

pub fn run(action: Action, host: &Host, out: &mut impl Write) -> Result<(), Error> {
    let mut system = System {
        root: PathBuf::from("/"),
        host: host.clone(),
        runner: Commands,
    };
    system.execute(action, out)
}

#[cfg(unix)]
pub(crate) fn text(path: &Path) -> Result<&str, Error> {
    match path.to_str() {
        Some(text) => Ok(text),
        None => Err(Error::Failed(format!(
            "{} is not valid UTF-8",
            path.display()
        ))),
    }
}

impl<R: Runner> System<R> {
    pub fn execute(&mut self, action: Action, out: &mut impl Write) -> Result<(), Error> {
        match action {
            Action::Install { init } => {
                let init = self.choose(init)?;
                self.install(init, out)
            }
            Action::Update => {
                let Registration::Registered(init) = self.registration()? else {
                    return Err(Error::NotInstalled);
                };
                self.update(init)
            }
            Action::Uninstall { purge } => self.uninstall(purge),
            Action::Status => {
                let status = self.status()?;
                serde_json::to_writer(&mut *out, &status)?;
                writeln!(out)?;
                Ok(())
            }
        }
    }

    pub fn paths(&self) -> Paths {
        #[cfg(windows)]
        {
            Paths::system()
        }
        #[cfg(not(windows))]
        {
            Paths::linux_under(&self.root)
        }
    }

    pub fn at(&self, logical: &Path) -> PathBuf {
        let relative: PathBuf = logical
            .components()
            .filter(|part| !matches!(part, Component::RootDir | Component::Prefix(..)))
            .collect();
        self.root.join(relative)
    }

    pub fn has_program(&self, name: &str) -> bool {
        Search::DIRS
            .iter()
            .any(|dir| self.at(&Path::new(dir).join(name)).is_file())
    }

    #[cfg(unix)]
    pub fn program(&self, name: &str) -> Result<String, Error> {
        for dir in Search::DIRS {
            let program = Path::new(dir).join(name);
            if self.at(&program).is_file() {
                return Ok(format!("{dir}/{name}"));
            }
        }
        Err(Error::Failed(format!(
            "{name} is not in {}",
            Search::DIRS.join(":")
        )))
    }

    #[cfg(unix)]
    pub fn exec(&mut self, name: &str, args: &[&str]) -> Result<Outcome, Error> {
        let program = self.program(name)?;
        self.runner.require(&program, args)
    }

    #[cfg(unix)]
    pub fn query(&mut self, name: &str, args: &[&str]) -> Result<Outcome, Error> {
        let program = self.program(name)?;
        self.runner.run(&program, args)
    }

    pub fn detect(&self) -> Result<Init, Error> {
        if cfg!(windows) {
            return Ok(Init::Windows);
        }
        if self.at(Path::new("/run/systemd/system")).is_dir() {
            return Ok(Init::Systemd);
        }
        if self.at(Path::new("/run/openrc")).is_dir() || self.has_program("openrc-run") {
            return Ok(Init::Openrc);
        }
        let runit = self.at(Path::new("/run/runit")).is_dir()
            || self.at(Path::new("/etc/runit/runsvdir")).is_dir();
        if runit && self.has_program("sv") {
            return Ok(Init::Runit);
        }
        Err(Error::NoInit)
    }

    pub fn choose(&self, choice: InitChoice) -> Result<Init, Error> {
        match choice {
            InitChoice::Auto => self.detect(),
            InitChoice::Systemd => Ok(Init::Systemd),
            InitChoice::Openrc => Ok(Init::Openrc),
            InitChoice::Runit => Ok(Init::Runit),
            InitChoice::Windows => Ok(Init::Windows),
        }
    }

    fn status(&mut self) -> Result<Status, Error> {
        match self.registration()? {
            Registration::Registered(init) => self.state(init),
            Registration::Absent => Ok(Status {
                init: self.detect()?,
                registered: false,
                enabled: false,
                running: false,
            }),
        }
    }
}
