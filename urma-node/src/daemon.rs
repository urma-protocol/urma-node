//! The archive daemon shared by `serve` and `urma-node service run`.
mod command;
mod inputs;
mod options;
mod overlay;
#[cfg(windows)]
mod scm;
mod serve;

pub use command::{ServiceCommand, run as run_service};
pub use inputs::{read_archive_config, read_header_bootstrap, read_peer_key, read_token};
pub use options::{OverlayOptions, ServeOptions};
pub use serve::serve;
