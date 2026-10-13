use crate::error::Error;
use std::path::PathBuf;

pub const CONFIG_LIMIT: usize = 16_384;
pub const TOKEN_LIMIT: usize = 256;

pub fn path() -> Result<PathBuf, Error> {
    match std::env::var_os("URMA_CONFIG") {
        Some(path) => Ok(PathBuf::from(path)),
        None => match std::env::var_os("HOME") {
            Some(home) => Ok(PathBuf::from(home).join(".config/urma/config.json")),
            None => Err(Error::Home),
        },
    }
}
