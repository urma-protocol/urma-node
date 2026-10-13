use crate::{Error as NodeError, platform::Paths};
use std::{fmt, io, path::Path};

#[derive(Debug)]
pub enum Error {
    NoInit,
    Failed(String),
    Io(io::Error),
    Json(serde_json::Error),
    Command {
        program: String,
        args: Vec<String>,
        detail: String,
    },
    NotInstalled,
    Account(String),
    Node(NodeError),
    #[cfg(windows)]
    Windows {
        call: &'static str,
        error: windows_service::Error,
    },
}

impl Error {
    fn no_init(f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        let paths = Paths::linux_under(Path::new("/"));
        write!(
            f,
            "No supported init system found (systemd, OpenRC, runit). Configure your service \
             manager to run `urma-node service run` as user urma-node.\n  \
             binaries: /usr/bin/urma-node, /usr/bin/urma-node-control, /usr/bin/urma-node-app\n  \
             service settings: {}\n  archive config: {}\n  data: {}\n  \
             token: {} (0640, urma-node:urma-node)\n  peer key: {} (0600)",
            paths.settings.display(),
            paths.config.display(),
            paths.data.display(),
            paths.token.display(),
            paths.peer_key.display(),
        )
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        match self {
            Self::NoInit => Self::no_init(f),
            Self::Failed(message) => f.write_str(message),
            Self::Io(error) => write!(f, "service install I/O: {error}"),
            Self::Json(error) => write!(f, "service status encoding: {error}"),
            Self::Command {
                program,
                args,
                detail,
            } => write!(f, "`{program} {}` failed: {detail}", args.join(" ")),
            Self::NotInstalled => {
                f.write_str("the service is not installed; run `urma-node service install`")
            }
            Self::Account(message) => write!(f, "service account: {message}"),
            Self::Node(error) => write!(f, "service defaults: {error}"),
            #[cfg(windows)]
            Self::Windows { call, error } => match error {
                windows_service::Error::Winapi(cause) => {
                    write!(f, "Windows service manager: {call}: {cause}")
                }
                other => write!(f, "Windows service manager: {call}: {other}"),
            },
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::Node(error) => Some(error),
            #[cfg(windows)]
            Self::Windows { error, .. } => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

impl From<NodeError> for Error {
    fn from(error: NodeError) -> Self {
        Self::Node(error)
    }
}
