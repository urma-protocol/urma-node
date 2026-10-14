//! `/dnsaddr` seeds are resolved by discovery and their records dialed one
//! at a time, QUIC first, TCP when QUIC does not answer (§3.1). Names resolve
//! through a fixed table; the peers are real PeerNetworks on loopback.
//! Unix only: wine refuses loopback libp2p dials with WSAEACCES.
#![cfg(unix)]
#[allow(dead_code)]
mod common;
#[allow(dead_code)]
#[path = "common/discovery_host.rs"]
mod discovery_host;
use discovery_host::{Node, bare, node, tcp_and_quic};
use libp2p::{Multiaddr, multiaddr::Protocol};
use std::{
    net::UdpSocket,
    path::Path,
    sync::{Arc, Mutex, atomic::AtomicBool},
};
use urma_node::{
    model::BookEntry,
    peer_discovery::Discovery,
    peer_seed::Names,
    peerbook::{Origin, PeerBook},
};

const LOOPBACK: Option<&str> = Some("127.0.0.1");

fn tcp(node: &Node) -> Multiaddr {
    bare(&node.listening()).with(Protocol::P2p(node.id))
}

fn quic(node: &Node) -> Multiaddr {
    tcp_and_quic(&node.listening())[1]
        .clone()
        .with(Protocol::P2p(node.id))
}

/// One discovery round of `dialer` whose only seed, `/dnsaddr/seed.test`,
/// resolves to `records` in that TXT order. Returns the seed's book entry.
fn seed_round(root: &Path, dialer: &Node, records: Vec<Multiaddr>) -> BookEntry {
    let path = root.join("a").join("peers.json");
    let mut book = PeerBook::load(&path, dialer.genesis()).unwrap();
    book.add("/dnsaddr/seed.test".parse().unwrap(), Origin::Seed)
        .unwrap();
    let discovery = Discovery::new(dialer.network.clone(), Arc::new(Mutex::new(book)), path)
        .with_names(Names::Fixed(vec![("seed.test".into(), records)]));
    discovery
        .step(&dialer.engine, &AtomicBool::new(false))
        .unwrap();
    let status = discovery.status().unwrap();
    assert_eq!(status.last_seed_round_error, None);
    discovery
        .book()
        .unwrap()
        .into_iter()
        .find(|entry| entry.addrs == ["/dnsaddr/seed.test"])
        .expect("seed stays in the book")
}

#[test]
fn a_seed_resolving_to_tcp_and_quic_is_dialed_over_quic() {
    let root = tempfile::tempdir().unwrap();
    let dialer = node(root.path(), "a", LOOPBACK);
    let (over_tcp, over_quic) = (
        node(root.path(), "b", LOOPBACK),
        node(root.path(), "c", LOOPBACK),
    );
    let seed = seed_round(root.path(), &dialer, vec![tcp(&over_tcp), quic(&over_quic)]);
    assert_eq!(
        dialer.connected(),
        vec![over_quic.id],
        "the QUIC record is dialed first although TXT lists TCP first"
    );
    assert_eq!((seed.failures, seed.last_success.is_some()), (0, true));
}

#[test]
fn a_seed_whose_quic_record_is_unreachable_is_reached_over_tcp() {
    let root = tempfile::tempdir().unwrap();
    let dialer = node(root.path(), "a", LOOPBACK);
    let (silent, over_tcp) = (
        node(root.path(), "b", LOOPBACK),
        node(root.path(), "c", LOOPBACK),
    );
    let closed = UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    let unreachable: Multiaddr = format!("/ip4/127.0.0.1/udp/{port}/quic-v1/p2p/{}", silent.id)
        .parse()
        .unwrap();
    let seed = seed_round(root.path(), &dialer, vec![tcp(&over_tcp), unreachable]);
    assert_eq!(dialer.connected(), vec![over_tcp.id]);
    assert_eq!((seed.failures, seed.last_success.is_some()), (0, true));
}
