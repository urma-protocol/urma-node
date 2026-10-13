use crate::config;
use crate::error::Error;
use bitcoin::BlockHash;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::PathBuf;
use urma_chain::observation::Chain;
use urma_node_client::Client;
use urma_node_client::model::{BookEntry, Omission, PruneReceipt, ServiceStatus};

#[derive(Deserialize)]
struct Settings {
    archive: Option<Archive>,
}

#[derive(Deserialize)]
struct Archive {
    endpoint: SocketAddr,
    token_file: PathBuf,
}

impl Settings {
    fn connect() -> Result<(Client, SocketAddr), Error> {
        let path = config::path()?;
        let missing = !path
            .try_exists()
            .map_err(|error| Error::Config(path.clone(), error.to_string()))?;
        if missing {
            return Err(Error::Unconfigured(path));
        }
        let bytes = urma_io::read_regular(&path, config::CONFIG_LIMIT)
            .map_err(|error| Error::Config(path.clone(), error.to_string()))?;
        let settings: Self = serde_json::from_slice(&bytes)
            .map_err(|error| Error::Config(path.clone(), error.to_string()))?;
        let Some(archive) = settings.archive else {
            return Err(Error::Unconfigured(path));
        };
        let token = urma_io::read_private(&archive.token_file, config::TOKEN_LIMIT)
            .map_err(|error| Error::Token(archive.token_file.clone(), error.to_string()))?;
        let token = std::str::from_utf8(&token)
            .map_err(|error| Error::Token(archive.token_file.clone(), error.to_string()))?;
        let client = Client::new(archive.endpoint, token.trim().to_owned())
            .map_err(|error| Error::Token(archive.token_file.clone(), error.to_string()))?;
        Ok((client, archive.endpoint))
    }
}

#[derive(Serialize)]
pub struct Overview {
    endpoint: SocketAddr,
    chain: Option<&'static str>,
    status: ServiceStatus,
}

#[tauri::command(async)]
pub fn node_status() -> Result<Overview, Error> {
    let (client, endpoint) = Settings::connect()?;
    let status = client
        .status()
        .map_err(|error| Error::node(endpoint, error))?;
    let chain = chain_label(status.archive.chain_genesis)?;
    Ok(Overview {
        endpoint,
        chain,
        status,
    })
}

#[tauri::command(async)]
pub fn prune_cache() -> Result<PruneReceipt, Error> {
    let (client, endpoint) = Settings::connect()?;
    client
        .prune_cache()
        .map_err(|error| Error::node(endpoint, error))
}

#[tauri::command(async)]
pub fn overlay_omissions() -> Result<Vec<Omission>, Error> {
    let (client, endpoint) = Settings::connect()?;
    client
        .overlay_omissions()
        .map_err(|error| Error::node(endpoint, error))
}

#[tauri::command(async)]
pub fn overlay_book() -> Result<Vec<BookEntry>, Error> {
    let (client, endpoint) = Settings::connect()?;
    client
        .overlay_book()
        .map_err(|error| Error::node(endpoint, error))
}

fn chain_label(genesis: BlockHash) -> Result<Option<&'static str>, Error> {
    for chain in [
        Chain::BitcoinMainnet,
        Chain::BitcoinRegtest,
        Chain::BitcoinTestnet4,
        Chain::LitecoinMainnet,
        Chain::LitecoinTestnet,
    ] {
        if chain.genesis()?.0 == genesis {
            return Ok(Some(chain.label()));
        }
    }
    Ok(None)
}
