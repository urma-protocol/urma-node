//! Built-in defaults that need one home. Seeds are DNS names whose records the
//! operator publishes; the daemon dials whatever they resolve to. Regtest and
//! Bitcoin test chains have no seeds.
use crate::Error;
use crate::model::{ArchiveConfig, ChainPoint, Retention};
use libp2p::Multiaddr;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use urma_chain::observation::Chain;
use urma_runtime::p2p::Anchor;

/// The product's directory under `%ProgramData%` and `%ProgramFiles%`.
pub const WINDOWS_PRODUCT_DIR: &str = "URMA Node";

/// The system service name: Windows service, Linux account and unit.
pub const SERVICE_NAME: &str = "urma-node";

/// The chain a fresh system install follows (URMA_NODE_INSTALL.md §1).
pub const SERVICE_CHAIN: Chain = Chain::LitecoinTestnet;

/// The loopback control API of the system service.
pub const SERVICE_LISTEN: SocketAddr =
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 28331));

/// Network time slice per background object round, as `serve` defaults it.
pub const SERVICE_OBJECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// The URMA overlay listener of the system service: TCP and QUIC (UDP).
pub const SERVICE_URMA_LISTEN: SocketAddr =
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 28332));

/// First-install archive config: the embedded header checkpoint of
/// [`SERVICE_CHAIN`] as application start, unpruned.
pub fn service_archive_config() -> Result<ArchiveConfig, Error> {
    let anchor =
        Anchor::embedded(SERVICE_CHAIN).map_err(|error| Error::Invalid(error.to_string()))?;
    Ok(ArchiveConfig {
        chain: SERVICE_CHAIN,
        application_start: ChainPoint {
            height: anchor.height,
            hash: anchor.header.block_hash(),
        },
        retention: Retention::Unpruned,
    })
}

/// `%ProgramData%`, where the Windows service keeps settings and state.
#[cfg(windows)]
pub fn program_data() -> std::path::PathBuf {
    match std::env::var_os("ProgramData") {
        Some(dir) => std::path::PathBuf::from(dir),
        None => std::path::PathBuf::from(r"C:\ProgramData"),
    }
}

#[cfg(windows)]
pub fn program_files() -> std::path::PathBuf {
    match std::env::var_os("ProgramFiles") {
        Some(dir) => std::path::PathBuf::from(dir),
        None => std::path::PathBuf::from(r"C:\Program Files"),
    }
}

pub const SEEDS: &[(Chain, &str)] = &[
    (Chain::LitecoinMainnet, "/dnsaddr/ltc-seed.urma.rosint.org"),
    (Chain::LitecoinTestnet, "/dnsaddr/tltc-seed.urma.rosint.org"),
];

/// Parsed built-in seeds for `chain`.
pub fn seeds(chain: Chain) -> Result<Vec<Multiaddr>, Error> {
    let genesis = |chain: Chain| {
        chain
            .genesis()
            .map(|identity| identity.0)
            .map_err(|error| Error::Invalid(error.to_string()))
    };
    let wanted = genesis(chain)?;
    let mut seeds = Vec::new();
    for (seeded, address) in SEEDS {
        if genesis(*seeded)? == wanted {
            let parsed = address
                .parse()
                .map_err(|error| Error::Invalid(format!("built-in seed {address}: {error}")))?;
            seeds.push(parsed);
        }
    }
    Ok(seeds)
}
