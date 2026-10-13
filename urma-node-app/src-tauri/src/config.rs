use crate::error::Error;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::PathBuf;

pub const CONFIG_LIMIT: usize = 16_384;
pub const TOKEN_LIMIT: usize = 256;

pub const SERVICE_ENDPOINT: SocketAddr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 28331));

pub enum Override {
    At(PathBuf),
    Unset,
}

pub fn developer_override() -> Override {
    match std::env::var_os("URMA_CONFIG") {
        Some(path) => Override::At(PathBuf::from(path)),
        None => match std::env::var_os("HOME") {
            Some(home) => Override::At(PathBuf::from(home).join(".config/urma/config.json")),
            None => Override::Unset,
        },
    }
}

#[cfg(not(windows))]
pub fn service_token() -> Result<PathBuf, Error> {
    Ok(PathBuf::from("/var/lib/urma-node/service.token"))
}

#[cfg(windows)]
pub fn service_token() -> Result<PathBuf, Error> {
    match std::env::var_os("ProgramData") {
        Some(data) => Ok(PathBuf::from(data).join("URMA Node").join("service.token")),
        None => Err(Error::ProgramData),
    }
}
