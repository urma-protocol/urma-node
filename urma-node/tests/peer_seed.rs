//! `/dnsaddr` seeds are resolved by discovery and their public records dialed
//! one at a time, QUIC first, TCP when QUIC does not answer (§3.1), all
//! within one operation's time. Names resolve through a fixed table; the
//! peers are real PeerNetworks on the public-looking addresses of a private
//! namespace (see common/discovery_host.rs).
#[allow(dead_code)]
mod common;
#[allow(dead_code)]
#[path = "common/discovery_host.rs"]
mod discovery_host;
use discovery_host::{Node, PUBLIC, bare, host, node, report, run, tcp_and_quic};
use libp2p::{Multiaddr, PeerId, multiaddr::Protocol};
use serde_json::{Value, json};
use std::{
    net::UdpSocket,
    path::Path,
    sync::{Arc, Mutex, atomic::AtomicBool},
    time::Instant,
};
use urma_node::{
    model::BookEntry,
    peer_discovery::Discovery,
    peer_seed::Names,
    peerbook::{Origin, PeerBook},
};

const HOST: &str = "peer_seed_host";

fn tcp(node: &Node) -> Multiaddr {
    bare(&node.listening()).with(Protocol::P2p(node.id))
}

fn quic(node: &Node) -> Multiaddr {
    tcp_and_quic(&node.listening())[1]
        .clone()
        .with(Protocol::P2p(node.id))
}

/// A QUIC record on `ip` whose UDP port nothing answers: the handshake runs
/// into its timeout.
fn black_hole(ip: &str, peer: PeerId) -> Multiaddr {
    let closed = UdpSocket::bind((ip, 0)).unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    format!("/ip4/{ip}/udp/{port}/quic-v1/p2p/{peer}")
        .parse()
        .unwrap()
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
    discovery
        .book()
        .unwrap()
        .into_iter()
        .find(|entry| entry.addrs == ["/dnsaddr/seed.test"])
        .expect("seed stays in the book")
}

/// TXT lists the TCP record of one peer before the QUIC record of another:
/// the QUIC one is dialed.
fn quic_first(root: &Path) -> Value {
    let dialer = node(root, "a", Some(PUBLIC[0]));
    let over_tcp = node(root, "b", Some(PUBLIC[1]));
    let over_quic = node(root, "c", Some(PUBLIC[2]));
    let seed = seed_round(root, &dialer, vec![tcp(&over_tcp), quic(&over_quic)]);
    json!({
        "quic": dialer.connected() == vec![over_quic.id],
        "failures": seed.failures,
        "reached": seed.last_success.is_some(),
    })
}

fn fallback(root: &Path) -> Value {
    let dialer = node(root, "a", Some(PUBLIC[0]));
    let silent = node(root, "b", Some(PUBLIC[1]));
    let over_tcp = node(root, "c", Some(PUBLIC[2]));
    let records = vec![tcp(&over_tcp), black_hole(PUBLIC[1], silent.id)];
    let seed = seed_round(root, &dialer, records);
    json!({
        "tcp": dialer.connected() == vec![over_tcp.id],
        "failures": seed.failures,
        "reached": seed.last_success.is_some(),
    })
}

/// Four black-holed QUIC records cost one operation's time, not four.
fn deadline(root: &Path) -> Value {
    let dialer = node(root, "a", Some(PUBLIC[0]));
    let records = (0..4)
        .map(|index| black_hole(PUBLIC[1 + index % 2], PeerId::random()))
        .collect();
    let started = Instant::now();
    let seed = seed_round(root, &dialer, records);
    json!({
        "seconds": started.elapsed().as_secs(),
        "connected": dialer.connected().len(),
        "reached": seed.last_success.is_some(),
    })
}

#[test]
#[ignore = "owned namespaced subprocess host, invoked only by the seed cases"]
fn peer_seed_host() {
    let (root, scenario) = host();
    let checks = match scenario.as_str() {
        "quic_first" => quic_first(&root),
        "fallback" => fallback(&root),
        "deadline" => deadline(&root),
        _ => panic!("unknown owned scenario"),
    };
    report(&root, &scenario, checks);
}

#[test]
fn a_seed_resolving_to_tcp_and_quic_is_dialed_over_quic() {
    let checks = run(HOST, "quic_first");
    assert_eq!(checks["quic"], true, "TXT lists TCP first: {checks}");
    assert_eq!(
        (checks["failures"].clone(), checks["reached"].clone()),
        (json!(0), json!(true))
    );
}

#[test]
fn a_seed_whose_quic_record_is_unreachable_is_reached_over_tcp() {
    let checks = run(HOST, "fallback");
    assert_eq!(checks["tcp"], true, "{checks}");
    assert_eq!(
        (checks["failures"].clone(), checks["reached"].clone()),
        (json!(0), json!(true))
    );
}

#[test]
fn black_holed_seed_records_share_one_operation_deadline() {
    let checks = run(HOST, "deadline");
    let seconds = checks["seconds"].as_u64().unwrap();
    assert!(
        seconds <= Discovery::OPERATION_TIME.as_secs() + 2,
        "four 5 s QUIC handshakes took {seconds} s: {checks}"
    );
    assert_eq!(
        (checks["connected"].clone(), checks["reached"].clone()),
        (json!(0), json!(false))
    );
}
