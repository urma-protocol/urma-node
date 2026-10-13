//! Placeholder for U2's install implementation; the signatures are the contract.
use std::{ffi::OsString, fmt, io::Write};

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum InitChoice {
    Auto,
    Systemd,
    Openrc,
    Runit,
    Windows,
}

#[derive(Clone, Debug, clap::Subcommand)]
pub enum Action {
    /// Register, configure and start the system service.
    Install {
        #[arg(long, value_enum, default_value = "auto")]
        init: InitChoice,
    },
    /// Rewrite the service registration and restart it.
    Update,
    /// Stop and remove the service registration.
    Uninstall {
        /// Also remove data, configuration and the service account.
        #[arg(long)]
        purge: bool,
    },
    /// Print init kind, registration and run state as JSON.
    Status,
}

/// What main.rs reads from the environment for install; empty = absent.
#[derive(Clone, Debug, Default)]
pub struct Host {
    pub sudo_user: String,
    pub pkexec_uid: String,
    pub path: OsString,
}

#[derive(Debug)]
pub enum Error {
    /// No supported init system: the process exits with code 3.
    NoInit,
    Failed(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoInit => f.write_str("no supported init system found"),
            Self::Failed(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for Error {}

pub fn run(action: Action, _host: &Host, _out: &mut impl Write) -> Result<(), Error> {
    Err(Error::Failed(format!(
        "service {action:?} is not implemented yet"
    )))
}
