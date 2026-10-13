//! `urma-node service install|update|uninstall|status`: init-system
//! integration (URMA_NODE_INSTALL.md §4). Owned by U2.
mod stub;

pub use stub::{Action, Error, Host, InitChoice, run};
