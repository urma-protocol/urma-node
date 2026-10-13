use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Debug)]
pub enum Error {
    #[cfg(windows)]
    ProgramData,
    NotInstalled(PathBuf),
    Access(PathBuf),
    Config(PathBuf, String),
    Token(PathBuf, String),
    Offline(SocketAddr, std::io::Error),
    Node(urma_node_client::Error),
    Genesis(bitcoin::hex::HexToArrayError),
}

impl Error {
    pub fn node(endpoint: SocketAddr, error: urma_node_client::Error) -> Self {
        match error {
            urma_node_client::Error::Io(error) => Self::Offline(endpoint, error),
            other => Self::Node(other),
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Self::NotInstalled(_) => "missing",
            Self::Access(_) => "access",
            #[cfg(windows)]
            Self::ProgramData => "config",
            Self::Config(..) | Self::Token(..) => "config",
            Self::Offline(..) => "offline",
            Self::Node(_) | Self::Genesis(_) => "node",
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(windows)]
            Self::ProgramData => f.write_str("ProgramData is unset"),
            Self::NotInstalled(path) => {
                write!(f, "URMA Node is not installed: no {}", path.display())
            }
            #[cfg(not(windows))]
            Self::Access(path) => write!(
                f,
                "No access to {}: add your user to group urma-node and log in again: sudo usermod -aG urma-node $USER",
                path.display()
            ),
            #[cfg(windows)]
            Self::Access(path) => write!(
                f,
                "No access to {}: run the URMA Node installer again",
                path.display()
            ),
            Self::Config(path, error) => write!(f, "{}: {error}", path.display()),
            Self::Token(path, error) => write!(f, "token {}: {error}", path.display()),
            Self::Offline(endpoint, error) => write!(f, "no node at {endpoint}: {error}"),
            Self::Node(error) => error.fmt(f),
            Self::Genesis(error) => write!(f, "chain genesis: {error}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<bitcoin::hex::HexToArrayError> for Error {
    fn from(error: bitcoin::hex::HexToArrayError) -> Self {
        Self::Genesis(error)
    }
}

impl Serialize for Error {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut failure = serializer.serialize_struct("Error", 2)?;
        failure.serialize_field("kind", self.kind())?;
        failure.serialize_field("message", &self.to_string())?;
        failure.end()
    }
}
