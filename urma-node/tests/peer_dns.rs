//! An unreadable system resolver configuration never stops the overlay: it
//! starts without DNS, IP-literal peers still connect, and named seeds are
//! skipped with the reason visible in discovery status. No public resolver.
#[allow(dead_code)]
mod common;
use common::{config, witness_block};
use libp2p::{Multiaddr, identity::Keypair};
use std::{
    path::Path,
    sync::{Arc, Mutex, atomic::AtomicBool},
};
use urma_node::{
    engine::Engine,
    model::Retention,
    peer::{Dns, DnsMode, PeerNetwork, ReachMode},
    peer_discovery::Discovery,
    peerbook::{Origin, PeerBook},
};

const INJECTED: &str = "injected: resolver configuration unreadable";

fn engine(directory: &Path) -> Arc<Engine> {
    let block = witness_block(&[], None, 4242);
    Arc::new(Engine::open(directory, config(&block, Retention::Pruned), None).unwrap())
}

fn listener() -> Option<Multiaddr> {
    Some("/ip4/127.0.0.1/tcp/0".parse().unwrap())
}

fn refused(engine: &Arc<Engine>) -> PeerNetwork {
    PeerNetwork::start_with(
        Arc::clone(engine),
        Keypair::generate_ed25519(),
        (listener(), Vec::new()),
        (DnsMode::Refuse(INJECTED.into()), ReachMode::AutoNat),
    )
    .unwrap()
}

#[test]
fn named_seeds_are_reported_and_never_penalized_without_a_resolver() {
    let root = tempfile::tempdir().unwrap();
    let engine = engine(&root.path().join("a"));
    let network = refused(&engine);
    assert_eq!(network.dns(), &Dns::Unavailable(INJECTED.into()));
    let book_path = root.path().join("a").join("peers.json");
    let genesis = engine.status().unwrap().chain_genesis;
    let mut book = PeerBook::load(&book_path, genesis).unwrap();
    book.add("/dnsaddr/seed.urma.invalid".parse().unwrap(), Origin::Seed)
        .unwrap();
    let discovery = Discovery::new(Arc::new(network), Arc::new(Mutex::new(book)), book_path);
    discovery.step(&engine, &AtomicBool::new(false)).unwrap();
    let status = discovery.status().unwrap();
    let error = status
        .last_seed_round_error
        .expect("seed round error reported");
    assert!(error.contains("/dnsaddr/seed.urma.invalid"), "{error}");
    assert!(error.contains("system DNS resolver unavailable"), "{error}");
    assert!(error.contains(INJECTED), "{error}");
    assert!(status.last_seed_round_error_at.is_some());
    let seed = discovery
        .book()
        .unwrap()
        .into_iter()
        .find(|entry| {
            entry
                .addrs
                .iter()
                .any(|address| address.contains("seed.urma.invalid"))
        })
        .expect("seed stays in the book");
    assert_eq!(seed.failures, 0, "an unresolvable seed is not penalized");
}

/// Unix only: wine refuses loopback libp2p dials with WSAEACCES.
#[test]
#[cfg(unix)]
fn ip_literal_peers_connect_without_a_resolver() {
    use std::time::{Duration, Instant};
    let root = tempfile::tempdir().unwrap();
    let network = refused(&engine(&root.path().join("a")));
    let resolving = PeerNetwork::start(
        engine(&root.path().join("b")),
        Keypair::generate_ed25519(),
        listener(),
        Vec::new(),
    )
    .unwrap();
    assert_eq!(resolving.dns(), &Dns::Available);
    let deadline = Instant::now() + Duration::from_secs(10);
    let reached = network
        .dial(
            resolving.listen_address().unwrap(),
            &AtomicBool::new(false),
            deadline,
        )
        .unwrap();
    assert_eq!(reached, resolving.peer_id());
}
