use crate::{
    cache, config,
    platform::{CommitRpc, ServiceSettings},
};
use libp2p::Multiaddr;
use std::{net::SocketAddr, path::PathBuf, time::Duration};

/// Everything `serve` runs with, whether it came from flags or service.json.
#[derive(Clone, Debug)]
pub struct ServeOptions {
    pub data: PathBuf,
    pub config: PathBuf,
    pub listen: SocketAddr,
    pub token_file: PathBuf,
    pub p2p: bool,
    pub header_bootstrap: Option<PathBuf>,
    pub chain_peers: Vec<SocketAddr>,
    pub commit_rpc: Option<CommitRpc>,
    pub commit_public: bool,
    pub overlay: OverlayOptions,
    pub cache_bytes: u64,
    pub object_timeout: Duration,
}

/// The URMA overlay; without a key it is off and does no key or book I/O.
#[derive(Clone, Debug)]
pub struct OverlayOptions {
    pub key: Option<PathBuf>,
    pub listen: Option<SocketAddr>,
    pub manual: Vec<Multiaddr>,
    pub seeds: Vec<Multiaddr>,
    pub builtin_seeds: bool,
    pub announce: Vec<Multiaddr>,
}

impl ServeOptions {
    /// The installed service: chain sync, overlay with built-in seeds and the
    /// public commit providers unless service.json turns them off.
    pub fn from_settings(settings: ServiceSettings) -> Self {
        Self {
            data: settings.data,
            config: settings.config,
            listen: settings.listen,
            token_file: settings.token,
            p2p: true,
            header_bootstrap: None,
            chain_peers: settings.chain_peers,
            commit_rpc: settings.commit_rpc,
            commit_public: settings.commit_public,
            overlay: OverlayOptions {
                key: Some(settings.peer_key),
                listen: settings.urma_listen,
                manual: Vec::new(),
                seeds: Vec::new(),
                builtin_seeds: true,
                announce: settings.urma_announce,
            },
            cache_bytes: cache::DEFAULT_CACHE_BYTES,
            object_timeout: config::SERVICE_OBJECT_TIMEOUT,
        }
    }
}
