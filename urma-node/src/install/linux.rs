use crate::{
    install::{
        Error, Init, Registration, Runner, Status, System,
        account::Account,
        ownership::{LogWriter, hand_over, installer, log_directory},
        system::text,
    },
    platform::{Paths, ensure_defaults},
};
use std::{
    fs::{self, Permissions},
    io::Write,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdapterFile {
    pub path: PathBuf,
    pub content: String,
    pub mode: u32,
}

pub struct Adapter;

impl Adapter {
    pub const BINARY: &'static str = "/usr/bin/urma-node";
    pub const LOG_DIR: &'static str = "/var/log/urma-node";
    const UNIT: &'static str = "urma-node.service";
    const SYSTEMD_UNIT: &'static str = "/etc/systemd/system/urma-node.service";
    const OPENRC_SCRIPT: &'static str = "/etc/init.d/urma-node";
    const OPENRC_ENABLED: &'static str = "/etc/runlevels/default/urma-node";
    const RUNIT_DIR: &'static str = "/etc/sv/urma-node";
    const RUNIT_RUN: &'static str = "/etc/sv/urma-node/run";
    const RUNIT_LOG_RUN: &'static str = "/etc/sv/urma-node/log/run";
    const RUNIT_LOG_SERVICE: &'static str = "/etc/sv/urma-node/log";
    const RUNIT_SUPERVISED: &'static str = "/etc/sv/urma-node/supervise/ok";
    const SUPERVISED_STATES: [&'static str; 3] = ["run:", "down:", "finish:"];
    const RUNSV_POLLS: u32 = 40;
    const RUNSV_POLL: Duration = Duration::from_millis(250);
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
    const WRITABLE_BY_OTHERS: u32 = 0o022;
}

fn foreign() -> Error {
    Error::Failed("the Windows service manager exists only on Windows".into())
}

fn remove_present(path: &Path) -> Result<(), Error> {
    if path.is_symlink() || path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
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
        let binary = text(&self.host.binary)?;
        let state = text(&state)?;
        let fill = |template: &str| {
            template
                .replace("@BIN@", binary)
                .replace("@STATE@", state)
                .replace("@LOG@", Adapter::LOG_DIR)
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

    pub fn trusted_binary(&self) -> Result<(), Error> {
        let expected = Path::new(Adapter::BINARY);
        let refuse = || {
            Error::Failed(format!(
                "refusing to register {}: the service binary must be {}, a regular file owned by \
                 root and writable by nobody else; install it there first",
                self.host.binary.display(),
                Adapter::BINARY
            ))
        };
        if self.host.binary != expected {
            return Err(refuse());
        }
        let meta = fs::symlink_metadata(self.at(expected))?;
        let sound = meta.is_file()
            && meta.uid() == installer()?
            && meta.mode() & Adapter::WRITABLE_BY_OTHERS == 0;
        match sound {
            true => Ok(()),
            false => Err(refuse()),
        }
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
        self.trusted_binary()?;
        let ids = self.ensure_account()?;
        let paths = self.paths();
        let created = ensure_defaults(&paths)?;
        hand_over(&created, &paths, ids)?;
        let log = self.at(Path::new(Adapter::LOG_DIR));
        match init {
            Init::Openrc => log_directory(&log, LogWriter::Root, ids)?,
            Init::Runit => log_directory(&log, LogWriter::Service, ids)?,
            Init::Systemd | Init::Windows => {}
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
        self.trusted_binary()?;
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
            let log = self.at(Path::new(Adapter::LOG_DIR));
            for dir in [&paths.settings_dir, &paths.state_dir, &log] {
                if dir.is_dir() && !dir.is_symlink() {
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
                self.exec("systemctl", &["daemon-reload"])?;
                self.exec("systemctl", &["enable", "--now", Adapter::UNIT])?;
            }
            Init::Openrc => {
                self.exec("rc-update", &["add", Account::NAME, "default"])?;
                self.exec("rc-service", &[Account::NAME, "start"])?;
            }
            Init::Runit => self.link_runit()?,
            Init::Windows => return Err(foreign()),
        }
        Ok(())
    }

    fn restart(&mut self, init: Init) -> Result<(), Error> {
        match init {
            Init::Systemd => {
                self.exec("systemctl", &["daemon-reload"])?;
                self.exec("systemctl", &["restart", Adapter::UNIT])?;
            }
            Init::Openrc => {
                self.exec("rc-service", &[Account::NAME, "restart"])?;
            }
            Init::Runit => match self.runit_links()?.is_empty() {
                true => self.link_runit()?,
                false => {
                    self.exec("sv", &["restart", Adapter::RUNIT_DIR])?;
                }
            },
            Init::Windows => return Err(foreign()),
        }
        Ok(())
    }

    fn remove(&mut self, init: Init) -> Result<(), Error> {
        match init {
            Init::Systemd => {
                self.exec("systemctl", &["disable", "--now", Adapter::UNIT])?;
                remove_present(&self.at(Path::new(Adapter::SYSTEMD_UNIT)))?;
                self.exec("systemctl", &["daemon-reload"])?;
            }
            Init::Openrc => {
                self.query("rc-service", &[Account::NAME, "stop"])?;
                if self.at(Path::new(Adapter::OPENRC_ENABLED)).is_symlink() {
                    self.exec("rc-update", &["del", Account::NAME, "default"])?;
                }
                remove_present(&self.at(Path::new(Adapter::OPENRC_SCRIPT)))?;
            }
            Init::Runit => {
                let supervised = self.at(Path::new(Adapter::RUNIT_SUPERVISED)).exists();
                if supervised {
                    self.exec("sv", &["-w", "30", "down", Adapter::RUNIT_DIR])?;
                }
                for link in self.runit_links()? {
                    remove_present(&link)?;
                }
                if supervised {
                    self.query("sv", &["exit", Adapter::RUNIT_LOG_SERVICE])?;
                    self.query("sv", &["exit", Adapter::RUNIT_DIR])?;
                    self.await_runsv_exit()?;
                }
                let dir = self.at(Path::new(Adapter::RUNIT_DIR));
                if dir.is_dir() && !dir.is_symlink() {
                    fs::remove_dir_all(dir)?;
                }
            }
            Init::Windows => return Err(foreign()),
        }
        Ok(())
    }

    fn await_runsv_exit(&mut self) -> Result<(), Error> {
        for _ in 0..Adapter::RUNSV_POLLS {
            let status = self.query("sv", &["status", Adapter::RUNIT_DIR])?;
            let supervised = Adapter::SUPERVISED_STATES
                .iter()
                .any(|state| status.stdout.starts_with(state));
            if !supervised {
                return Ok(());
            }
            thread::sleep(Adapter::RUNSV_POLL);
        }
        Err(Error::Failed(format!(
            "runsv for {} did not exit; remove it once runsvdir has dropped the service",
            Adapter::RUNIT_DIR
        )))
    }

    fn runit_links(&self) -> Result<Vec<PathBuf>, Error> {
        let mut parents: Vec<PathBuf> = Vec::new();
        for dir in Adapter::RUNIT_SERVICE_DIRS {
            let active = self.at(Path::new(dir));
            if !active.is_dir() {
                continue;
            }
            let parent = fs::canonicalize(&active)?;
            if !parents.contains(&parent) {
                parents.push(parent);
            }
        }
        Ok(parents
            .into_iter()
            .map(|parent| parent.join(Account::NAME))
            .filter(|link| link.is_symlink())
            .collect())
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
                let enabled = self.query("systemctl", &["is-enabled", Adapter::UNIT])?;
                let active = self.query("systemctl", &["is-active", Adapter::UNIT])?;
                (
                    enabled.stdout.trim() == "enabled",
                    active.stdout.trim() == "active",
                )
            }
            Init::Openrc => {
                let enabled = self.at(Path::new(Adapter::OPENRC_ENABLED)).is_symlink();
                let status = self.query("rc-service", &[Account::NAME, "status"])?;
                (enabled, status.success)
            }
            Init::Runit => {
                let status = self.query("sv", &["status", Adapter::RUNIT_DIR])?;
                (
                    !self.runit_links()?.is_empty(),
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
            listening: false,
        })
    }
}
