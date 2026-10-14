//! Optional archive engine. Opening an archive performs no network operations.
//! Applications may use their existing sources without depending on this crate
//! or running the standalone service.

pub mod archive;
pub mod cache;
pub mod client;
pub mod config;
pub mod daemon;
pub mod engine;
mod engine_sync;
mod error;
pub mod install;
pub mod model;
pub mod names;
pub mod peer;
pub mod peer_codec;
pub mod peer_discovery;
mod peer_fence;
pub mod peer_gossip;
mod peer_inventory;
mod peer_limits;
pub mod peer_live;
pub mod peer_nat;
mod peer_parent;
pub mod peer_protocol;
mod peer_rate;
mod peer_reach;
pub mod peer_sync;
mod peer_transport;
pub mod peerbook;
pub mod platform;
pub mod projection;
mod recorded;
pub mod retrieval;
mod scan;
pub mod service;
pub mod source;

pub use error::{DialLimit, Error};
