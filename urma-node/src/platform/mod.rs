//! Per-OS service paths, settings and first-install state (URMA_NODE_INSTALL.md §2–§3).
mod defaults;
mod exposure;
mod paths;
mod service_sid;
mod settings;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
use unix as os;
#[cfg(windows)]
use windows as os;

pub use defaults::{ensure_defaults, write_peer_key};
pub use exposure::Exposure;
pub use paths::Paths;
pub use service_sid::service_sid;
pub use settings::{CommitRpc, ServiceSettings};
