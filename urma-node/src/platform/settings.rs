//! `service.json`: what `urma-node service run` serves with
//! (URMA_NODE_INSTALL.md §3).
use super::paths::Paths;
use crate::{Error, config};
use libp2p::Multiaddr;
use serde::{Deserialize, Serialize};
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitRpc {
    pub url: String,
    pub cookie: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceSettings {
    pub data: PathBuf,
    pub config: PathBuf,
    pub token: PathBuf,
    pub peer_key: PathBuf,
    pub listen: SocketAddr,
    pub urma_listen: Option<SocketAddr>,
    pub urma_announce: Vec<Multiaddr>,
    pub chain_peers: Vec<SocketAddr>,
    pub commit_rpc: Option<CommitRpc>,
    pub commit_public: bool,
}

impl ServiceSettings {
    const LIMIT: usize = 64 * 1024;

    pub fn defaults(paths: &Paths) -> Self {
        Self {
            data: paths.data.clone(),
            config: paths.config.clone(),
            token: paths.token.clone(),
            peer_key: paths.peer_key.clone(),
            listen: config::SERVICE_LISTEN,
            urma_listen: Some(config::SERVICE_URMA_LISTEN),
            urma_announce: Vec::new(),
            chain_peers: Vec::new(),
            commit_rpc: None,
            commit_public: true,
        }
    }

    pub fn read(path: &Path) -> Result<Self, Error> {
        let bytes = urma_io::read_regular(path, Self::LIMIT).map_err(std::io::Error::other)?;
        Ok(serde_json::from_slice(&bytes)?)
    }
}
