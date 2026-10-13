use crate::{Error, model::ArchiveConfig};
use libp2p::identity::{KeyType, Keypair};
use std::{fmt::Display, io, path::Path};
use urma_chain::observation::Chain;
use urma_runtime::p2p::{HeaderBootstrap, context_headers};

struct Limits;

impl Limits {
    const CONFIG: usize = 64 * 1024;
    const BOOTSTRAP: usize = 1024 * 1024;
    const TOKEN: usize = 128;
    const PEER_KEY: usize = 4096;
}

pub(super) fn invalid(error: impl Display) -> Error {
    Error::Invalid(error.to_string())
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct HeaderBootstrapFile {
    version: u32,
    chain: Chain,
    height: u64,
    header: String,
    ancestors: Vec<String>,
}

pub fn read_archive_config(path: &Path) -> Result<ArchiveConfig, Error> {
    let bytes = match urma_io::read_admin(path, Limits::CONFIG) {
        Ok(bytes) => bytes,
        Err(urma_io::Error::TooLarge { .. }) => {
            return Err(Error::Invalid("config exceeds 64 KiB".into()));
        }
        Err(urma_io::Error::PrivatePermissions) => {
            return Err(Error::Invalid(format!(
                "{} must be owned by and writable only by administrators",
                path.display()
            )));
        }
        Err(error) => return Err(io::Error::other(error).into()),
    };
    Ok(serde_json::from_slice(&bytes)?)
}

pub fn read_token(path: &Path) -> Result<String, Error> {
    let bytes = match urma_io::read_shared(path, Limits::TOKEN) {
        Ok(bytes) => bytes,
        Err(urma_io::Error::PrivatePermissions) => {
            return Err(Error::Invalid(
                "service token must not be writable by group or open to others".into(),
            ));
        }
        Err(error) => return Err(invalid(format!("invalid service token file: {error}"))),
    };
    Ok(std::str::from_utf8(&bytes)
        .map_err(invalid)?
        .trim()
        .to_string())
}

pub fn read_peer_key(path: &Path) -> Result<Keypair, Error> {
    if !std::fs::symlink_metadata(path)?.is_file() {
        return Err(Error::Invalid(
            "peer key must be a regular private file".into(),
        ));
    }
    let bytes = urma_io::read_private(path, Limits::PEER_KEY).map_err(invalid)?;
    let key = Keypair::from_protobuf_encoding(&bytes).map_err(invalid)?;
    if key.key_type() != KeyType::Ed25519 {
        return Err(Error::Invalid("peer identity must be Ed25519".into()));
    }
    Ok(key)
}

pub fn read_header_bootstrap(
    path: &Path,
    chain: Chain,
    application_height: u64,
) -> Result<HeaderBootstrap, Error> {
    let bytes = match urma_io::read_bounded(path, Limits::BOOTSTRAP) {
        Ok(bytes) => bytes,
        Err(urma_io::Error::TooLarge { .. }) => {
            return Err(Error::Invalid("header bootstrap exceeds 1 MiB".into()));
        }
        Err(error) => return Err(io::Error::other(error).into()),
    };
    let input: HeaderBootstrapFile = serde_json::from_slice(&bytes).map_err(invalid)?;
    if input.version != 1
        || input.chain.genesis().map_err(invalid)? != chain.genesis().map_err(invalid)?
    {
        return Err(Error::Invalid(
            "header bootstrap version or chain mismatch".into(),
        ));
    }
    if input.height > application_height {
        return Err(Error::Invalid(
            "application start precedes the supplied header anchor".into(),
        ));
    }
    let context = u64::try_from(context_headers(chain)).map_err(invalid)?;
    let count = usize::try_from(input.height.min(context)).map_err(invalid)?;
    if input.ancestors.len() != count {
        return Err(Error::Invalid(format!(
            "header bootstrap needs exactly {count} ancestors"
        )));
    }
    let anchor = header(&input.header)?;
    let ancestors = input
        .ancestors
        .iter()
        .map(|raw| header(raw))
        .collect::<Result<Vec<_>, _>>()?;
    HeaderBootstrap::new(chain, input.height, anchor, ancestors).map_err(invalid)
}

fn header(raw: &str) -> Result<bitcoin::block::Header, Error> {
    if raw.len() != 160 {
        return Err(Error::Invalid(
            "bootstrap header must be exactly 80 bytes encoded as hex".into(),
        ));
    }
    bitcoin::consensus::deserialize(&hex::decode(raw).map_err(invalid)?).map_err(invalid)
}

pub(super) fn private_directory(path: &Path) -> Result<(), Error> {
    match urma_io::create_private_directory(path) {
        Ok(()) => Ok(()),
        Err(error)
            if error.kind() == io::ErrorKind::AlreadyExists
                && std::fs::symlink_metadata(path)?.is_dir() =>
        {
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}
