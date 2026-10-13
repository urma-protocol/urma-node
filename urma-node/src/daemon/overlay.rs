use super::{OverlayOptions, inputs::read_peer_key};
use crate::{
    Error, config,
    engine::Engine,
    peer::{OverlayCommitSource, PeerNetwork, SyncPeers},
    peer_discovery::Discovery,
    peer_sync::{OverlayInfo, PeerSync},
    peerbook::{Origin, PeerAddress, PeerBook},
    service::Service,
};
use libp2p::{Multiaddr, multiaddr::Protocol};
use std::{
    io::Write,
    net::IpAddr,
    path::Path,
    sync::{Arc, Mutex},
};
use urma_chain::observation::Chain;

pub(super) struct Attached {
    pub(super) service: Service,
    pub(super) network: Option<Arc<PeerNetwork>>,
    pub(super) commit_source: Option<OverlayCommitSource>,
}

/// Manual hints, then built-in and explicit seeds, merged into the saved book.
fn load_book(
    path: &Path,
    engine: &Engine,
    chain: Chain,
    options: &OverlayOptions,
) -> Result<PeerBook, Error> {
    let mut book = PeerBook::load(path, engine.status()?.chain_genesis)?;
    let mut changed = false;
    for address in &options.manual {
        changed |= book.add(address.clone(), Origin::Manual)?;
    }
    let builtin = match options.builtin_seeds {
        true => config::seeds(chain)?,
        false => Vec::new(),
    };
    for seed in builtin.into_iter().chain(options.seeds.iter().cloned()) {
        changed |= book.add(seed, Origin::Seed)?;
    }
    if changed {
        book.save(path)?;
    }
    Ok(book)
}

pub(super) fn attach_overlay(
    service: Service,
    data: &Path,
    engine: &Arc<Engine>,
    chain: Chain,
    (options, commit_parents): (OverlayOptions, bool),
    log: &mut impl Write,
) -> Result<Attached, Error> {
    let Some(path) = &options.key else {
        return Ok(Attached {
            service,
            network: None,
            commit_source: None,
        });
    };
    let identity = read_peer_key(path)?;
    let book_path = data.join("peers.json");
    let book = load_book(&book_path, engine, chain, &options)?;
    let listener = options.listen.map(|socket| {
        let mut address = Multiaddr::empty();
        address.push(match socket.ip() {
            IpAddr::V4(ip) => Protocol::Ip4(ip),
            IpAddr::V6(ip) => Protocol::Ip6(ip),
        });
        address.push(Protocol::Tcp(socket.port()));
        address
    });
    let announce: Vec<String> = options.announce.iter().map(Multiaddr::to_string).collect();
    let network = Arc::new(PeerNetwork::start(
        Arc::clone(engine),
        identity,
        listener,
        options.announce,
    )?);
    let book_size = book.stats().book;
    let manual: Vec<PeerAddress> = options.manual.iter().filter_map(identified).collect();
    let peers = network.sync_peers(manual.clone());
    let service = service.with_record_peers(peers.clone());
    let service = match commit_parents {
        true => {
            let book = Arc::new(Mutex::new(book));
            service
                .with_peer_sync(peer_sync(&network, peers, book_size, announce))?
                .with_discovery(Discovery::new(Arc::clone(&network), book, book_path))?
        }
        false => service,
    };
    writeln!(
        log,
        "URMA peer identity {} listener {:?}",
        network.peer_id(),
        network.listen_address()
    )?;
    let commit_source = commit_parents.then(|| network.commit_source(chain, manual));
    Ok(Attached {
        service,
        network: Some(network),
        commit_source,
    })
}

/// A `--urma-peer` naming its identity over an IP literal is a configured
/// sync source from the start; any other form joins once discovery connects.
fn identified(address: &Multiaddr) -> Option<PeerAddress> {
    let peer = address.iter().find_map(|part| match part {
        Protocol::P2p(peer) => Some(peer),
        _ => None,
    })?;
    PeerAddress::new(peer, address.clone()).ok()
}

/// Inventory peers are re-read every step: configured, then live ones.
fn peer_sync(
    network: &Arc<PeerNetwork>,
    peers: SyncPeers,
    book_size: u64,
    announce: Vec<String>,
) -> PeerSync {
    let (connected, listening) = (Arc::clone(network), Arc::clone(network));
    PeerSync {
        peers,
        info: OverlayInfo {
            peer_id: network.peer_id().to_string(),
            listen: Arc::new(move || listening.listen_addresses()),
            announce,
            peer_book: book_size,
            connected: Arc::new(move || connected.connected_peers()),
        },
    }
}

/// The service owned the only other handle; it is released when run returns.
pub(super) fn shutdown_overlay(network: Arc<PeerNetwork>) -> Result<(), Error> {
    match Arc::try_unwrap(network) {
        Ok(network) => network.shutdown(),
        Err(shared) => Err(Error::Busy(format!(
            "overlay still shared by {} handles after service stop",
            Arc::strong_count(&shared)
        ))),
    }
}
