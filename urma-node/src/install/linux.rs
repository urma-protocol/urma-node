use crate::{
    install::{Error, Init, Registration, Runner, Status, System, account::Account, system::text},
    platform::Paths,
};
use std::{
    fs::{self, Permissions},
    io::Write,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdapterFile {
    pub path: PathBuf,
    pub content: String,
    pub mode: u32,
}

pub struct Adapter;

impl Adapter {
    const UNIT: &'static str = "urma-node.service";
    const SYSTEMD_UNIT: &'static str = "/etc/systemd/system/urma-node.service";
    const OPENRC_SCRIPT: &'static str = "/etc/init.d/urma-node";
    const OPENRC_ENABLED: &'static str = "/etc/runlevels/default/urma-node";
    const RUNIT_DIR: &'static str = "/etc/sv/urma-node";
    const RUNIT_RUN: &'static str = "/etc/sv/urma-node/run";
    const RUNIT_LOG_RUN: &'static str = "/etc/sv/urma-node/log/run";
    const RUNIT_SUPERVISED: &'static str = "/etc/sv/urma-node/supervise/ok";
    const RUNIT_SERVICE_DIRS: [&'static str; 3] = [
        "/var/service",
        "/etc/runit/runsvdir/default",
        "/run/runit/service",
    ];
    const SYSTEMD: &'static str = include_str!("templates/urma-node.service");
    const OPENRC: &'static str = include_str!("templates/urma-node.openrc");
    const RUNIT: &'static str = include_str!("templates/runit-run");
    const RUNIT_LOG: &'static str = include_str!("templates/runit-log-run");
    const MARKERS: [(Init, &'static str); 3] = [
        (Init::Systemd, Self::SYSTEMD_UNIT),
        (Init::Openrc, Self::OPENRC_SCRIPT),
        (Init::Runit, Self::RUNIT_RUN),
    ];
}

fn log_dir() -> PathBuf {
    Paths::linux_under(Path::new("/")).state_dir.join("log")
}

fn foreign() -> Error {
    Error::Failed("the Windows service manager exists only on Windows".into())
}

fn file(path: &str, content: String, mode: u32) -> AdapterFile {
    AdapterFile {
        path: PathBuf::from(path),
        content,
        mode,
    }
}

impl<R: Runner> System<R> {
    pub fn render(&self, init: Init) -> Result<Vec<AdapterFile>, Error> {
        let state = Paths::linux_under(Path::new("/")).state_dir;
        let log = log_dir();
        let binary = text(&self.host.binary)?;
        let (state, log) = (text(&state)?, text(&log)?);
        let fill = |template: &str| {
            template
                .replace("@BIN@", binary)
                .replace("@STATE@", state)
                .replace("@LOG@", log)
        };
        Ok(match init {
            Init::Systemd => vec![file(Adapter::SYSTEMD_UNIT, fill(Adapter::SYSTEMD), 0o644)],
            Init::Openrc => vec![file(Adapter::OPENRC_SCRIPT, fill(Adapter::OPENRC), 0o755)],
            Init::Runit => vec![
                file(Adapter::RUNIT_RUN, fill(Adapter::RUNIT), 0o755),
                file(Adapter::RUNIT_LOG_RUN, fill(Adapter::RUNIT_LOG), 0o755),
            ],
            Init::Windows => return Err(foreign()),
        })
    }

    fn write_adapter(&self, init: Init) -> Result<(), Error> {
        for adapter in self.render(init)? {
            let path = self.at(&adapter.path);
            let Some(parent) = path.parent() else {
                return Err(Error::Failed(format!("{} has no parent", path.display())));
            };
            fs::create_dir_all(parent)?;
            fs::write(&path, adapter.content)?;
            fs::set_permissions(&path, Permissions::from_mode(adapter.mode))?;
        }
        Ok(())
    }

    pub fn registration(&self) -> Result<Registration, Error> {
        for (init, marker) in Adapter::MARKERS {
            if self.at(Path::new(marker)).is_file() {
                return Ok(Registration::Registered(init));
            }
        }
        Ok(Registration::Absent)
    }

    pub fn install(&mut self, init: Init, out: &mut impl Write) -> Result<(), Error> {
        self.render(init)?;
        let ids = self.ensure_account()?;
        let paths = self.paths();
        self.own(&paths, ids)?;
        if matches!(init, Init::Openrc | Init::Runit) {
            self.log_directory(&self.at(&log_dir()), ids)?;
        }
        let registration = self.registration()?;
        self.write_adapter(init)?;
        match registration {
            Registration::Registered(..) => self.restart(init)?,
            Registration::Absent => self.start(init)?,
        }
        self.join_group(out)
    }

    pub fn update(&mut self, init: Init) -> Result<(), Error> {
        self.write_adapter(init)?;
        self.restart(init)
    }

    pub fn uninstall(&mut self, purge: bool) -> Result<(), Error> {
        match self.registration()? {
            Registration::Registered(init) => self.remove(init)?,
            Registration::Absent => {}
        }
        if purge {
            let paths = self.paths();
            for dir in [&paths.settings_dir, &paths.state_dir] {
                if dir.is_dir() {
                    fs::remove_dir_all(dir)?;
                }
            }
            self.remove_account()?;
        }
        Ok(())
    }

    fn start(&mut self, init: Init) -> Result<(), Error> {
        match init {
            Init::Systemd => {
                self.runner.require("systemctl", &["daemon-reload"])?;
                self.runner
                    .require("systemctl", &["enable", "--now", Adapter::UNIT])?;
            }
            Init::Openrc => {
                self.runner
                    .require("rc-update", &["add", Account::NAME, "default"])?;
                self.runner
                    .require("rc-service", &[Account::NAME, "start"])?;
            }
            Init::Runit => self.link_runit()?,
            Init::Windows => return Err(foreign()),
        }
        Ok(())
    }

    fn restart(&mut self, init: Init) -> Result<(), Error> {
        match init {
            Init::Systemd => {
                self.runner.require("systemctl", &["daemon-reload"])?;
                self.runner
                    .require("systemctl", &["restart", Adapter::UNIT])?;
            }
            Init::Openrc => {
                self.runner
                    .require("rc-service", &[Account::NAME, "restart"])?;
            }
            Init::Runit => match self.runit_links().is_empty() {
                true => self.link_runit()?,
                false => {
                    self.runner
                        .require("sv", &["restart", Adapter::RUNIT_DIR])?;
                }
            },
            Init::Windows => return Err(foreign()),
        }
        Ok(())
    }

    fn remove(&mut self, init: Init) -> Result<(), Error> {
        match init {
            Init::Systemd => {
                self.runner
                    .require("systemctl", &["disable", "--now", Adapter::UNIT])?;
                fs::remove_file(self.at(Path::new(Adapter::SYSTEMD_UNIT)))?;
                self.runner.require("systemctl", &["daemon-reload"])?;
            }
            Init::Openrc => {
                self.runner.run("rc-service", &[Account::NAME, "stop"])?;
                if self.at(Path::new(Adapter::OPENRC_ENABLED)).is_symlink() {
                    self.runner
                        .require("rc-update", &["del", Account::NAME, "default"])?;
                }
                fs::remove_file(self.at(Path::new(Adapter::OPENRC_SCRIPT)))?;
            }
            Init::Runit => {
                if self.at(Path::new(Adapter::RUNIT_SUPERVISED)).exists() {
                    self.runner.require("sv", &["down", Adapter::RUNIT_DIR])?;
                }
                for link in self.runit_links() {
                    fs::remove_file(link)?;
                }
                fs::remove_dir_all(self.at(Path::new(Adapter::RUNIT_DIR)))?;
            }
            Init::Windows => return Err(foreign()),
        }
        Ok(())
    }

    fn runit_links(&self) -> Vec<PathBuf> {
        Adapter::RUNIT_SERVICE_DIRS
            .iter()
            .map(|dir| self.at(&Path::new(dir).join(Account::NAME)))
            .filter(|link| link.is_symlink())
            .collect()
    }

    fn link_runit(&self) -> Result<(), Error> {
        for dir in Adapter::RUNIT_SERVICE_DIRS {
            let active = self.at(Path::new(dir));
            if active.is_dir() {
                symlink(Adapter::RUNIT_DIR, active.join(Account::NAME))?;
                return Ok(());
            }
        }
        Err(Error::Failed(format!(
            "no active runit service directory ({})",
            Adapter::RUNIT_SERVICE_DIRS.join(", ")
        )))
    }

    pub fn state(&mut self, init: Init) -> Result<Status, Error> {
        let (enabled, running) = match init {
            Init::Systemd => {
                let enabled = self
                    .runner
                    .run("systemctl", &["is-enabled", Adapter::UNIT])?;
                let active = self
                    .runner
                    .run("systemctl", &["is-active", Adapter::UNIT])?;
                (
                    enabled.stdout.trim() == "enabled",
                    active.stdout.trim() == "active",
                )
            }
            Init::Openrc => {
                let enabled = self.at(Path::new(Adapter::OPENRC_ENABLED)).is_symlink();
                let status = self.runner.run("rc-service", &[Account::NAME, "status"])?;
                (enabled, status.success)
            }
            Init::Runit => {
                let status = self.runner.run("sv", &["status", Adapter::RUNIT_DIR])?;
                (
                    !self.runit_links().is_empty(),
                    status.stdout.starts_with("run:"),
                )
            }
            Init::Windows => return Err(foreign()),
        };
        Ok(Status {
            init,
            registered: true,
            enabled,
            running,
        })
    }
}
