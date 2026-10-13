#[cfg(unix)]
mod account;
mod cli;
mod error;
#[cfg(unix)]
mod linux;
mod runner;
mod system;
#[cfg(windows)]
mod windows;

pub use cli::{Action, Host, InitChoice};
pub use error::Error;
#[cfg(unix)]
pub use linux::AdapterFile;
pub use runner::{Commands, Outcome, Runner};
pub use system::{Init, Registration, Status, System, run};
