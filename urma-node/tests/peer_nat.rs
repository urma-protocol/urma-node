//! NAT traversal between real PeerNetworks in a private namespace (see
//! common/discovery_host.rs): QUIC beside TCP on one port, AutoNAT v2
//! reachability, relay reservations that outlive idle data connections, the
//! records gate on relayed connections, and a DCUtR upgrade to a direct link.
//! Reachability is forced where the namespace cannot model a NAT.
#![cfg(target_os = "linux")]
#[allow(dead_code)]
mod common;
#[allow(dead_code)]
#[path = "common/discovery_host.rs"]
mod discovery_host;
#[allow(dead_code)]
#[path = "common/nat_peer.rs"]
mod nat_peer;
use common::{MemorySource, config, witness_block};
use discovery_host::{
    Node, PUBLIC, bare, discover, host, node, offline, report, run, stage, until,
};
use libp2p::{Multiaddr, PeerId, identity::Keypair, multiaddr::Protocol};
use nat_peer::NatPeer;
use serde_json::{Value, json};
use std::{
    fs,
    net::TcpListener,
    path::Path,
    sync::{Arc, atomic::AtomicBool},
    thread,
    time::{Duration, Instant},
};
use urma_node::{
    engine::Engine,
    model::Retention,
    peer::{DnsMode, PeerLink, PeerNetwork, ReachMode, Reachability},
    peer_protocol::{Kind, Reply},
    peer_sync::{OverlayInfo, PeerSync},
    peerbook::{Origin, PeerBook},
    service::Service,
};

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

const HOST: &str = "peer_nat_host";
const PROBE: Duration = Duration::from_secs(30);
/// Longer than the 20 s idle timeout of data connections.
const IDLE: Duration = Duration::from_secs(45);

/// A node whose reachability is fixed. A public one announces its listener,
/// as AutoNAT would have confirmed it: relay reservations carry that address.
fn forced(root: &Path, name: &str, ip: &str, reachability: Reachability) -> Node {
    forced_on(offline(root, name), ip, reachability)
}

/// An archive with headers selected on the one-block chain `offline` names:
/// inventory sync contacts its peers instead of skipping every step.
fn chained(root: &Path, name: &str) -> Arc<Engine> {
    let directory = root.join(name);
    fs::create_dir_all(&directory).unwrap();
    let block = witness_block(&[], None, 1);
    let source = Arc::new(MemorySource::new(std::slice::from_ref(&block)));
    let config = config(&block, Retention::Unpruned);
    let engine = Engine::open(&directory, config, Some(source)).unwrap();
    engine.synchronize_headers().unwrap();
    Arc::new(engine)
}

fn forced_on(engine: Arc<Engine>, ip: &str, reachability: Reachability) -> Node {
    let key = Keypair::generate_ed25519();
    let id = key.public().to_peer_id();
    let port = TcpListener::bind((ip, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let listen: Multiaddr = format!("/ip4/{ip}/tcp/{port}").parse().unwrap();
    let announce = match reachability {
        Reachability::Public => vec![listen.clone()],
        Reachability::Private | Reachability::Unknown => Vec::new(),
    };
    let network = PeerNetwork::start_with(
        engine.clone(),
        key,
        (Some(listen), announce),
        (DnsMode::System, ReachMode::Forced(reachability)),
    )
    .unwrap();
    let network = Arc::new(network);
    let address = network.listen_address();
    Node {
        engine,
        network,
        id,
        address,
    }
}

fn tcp_port(address: &Multiaddr) -> Option<u16> {
    address.iter().find_map(|part| match part {
        Protocol::Tcp(port) => Some(port),
        _ => None,
    })
}

fn quic_port(address: &Multiaddr) -> Option<u16> {
    let quic = address.iter().any(|part| part == Protocol::QuicV1);
    address.iter().find_map(|part| match part {
        Protocol::Udp(port) if quic => Some(port),
        _ => None,
    })
}

fn link(node: &Node, peer: PeerId) -> Option<PeerLink> {
    let links = node.network.peer_links();
    links
        .into_iter()
        .find(|(id, _)| *id == peer)
        .map(|(_, link)| link)
}

fn quic(root: &Path) -> Value {
    let n = node(root, "n", Some(PUBLIC[0]));
    let port = tcp_port(&n.listening());
    stage(root, "QUIC listens on the TCP listener's port");
    let mut address = None;
    until(Duration::from_secs(5), || {
        address = n
            .network
            .listen_addresses()
            .into_iter()
            .find(|a| quic_port(a).is_some());
        address.is_some()
    });
    let address = address.expect("QUIC listener");
    stage(root, "a QUIC client is served records");
    let client = NatPeer::start(None, false);
    assert_eq!(client.dial(address.clone()), Ok(n.id));
    let served = matches!(
        client.call(n.id, Kind::Peers { limit: 0 }),
        Ok(Reply::Peers(_))
    );
    json!({"same_port":quic_port(&address) == port,"served":served,"link":link(&n, client.id).map(|l| l.as_str())})
}

fn autonat(root: &Path) -> Value {
    let server = node(root, "s", Some(PUBLIC[1]));
    let public = node(root, "a", Some(PUBLIC[0]));
    stage(root, "a dialable node is confirmed public");
    assert_eq!(public.dial(server.listening()), server.id);
    let confirmed = until(PROBE, || {
        public.network.reachability() == Reachability::Public
    });
    let external = public.network.external_addresses();
    stage(
        root,
        "a node without a listener is private and advertises nothing",
    );
    let hidden = node(root, "c", None);
    assert_eq!(hidden.dial(server.listening()), server.id);
    let private = until(PROBE, || {
        hidden.network.reachability() == Reachability::Private
    });
    json!({
        "public":confirmed,
        "external":external == vec![bare(&public.listening())],
        "private":private,
        "hidden_external":hidden.network.external_addresses().len(),
    })
}

/// The first AutoNAT result `client` gets, within the probe bound.
fn probed(client: &NatPeer) -> Option<(Multiaddr, bool)> {
    until(PROBE, || !client.probes().is_empty());
    client.probes().into_iter().next()
}

/// N listens everywhere. A client reaching it over loopback is observed at
/// 127.0.0.1 and asks for a dial-back there: N refuses before dialing, though
/// the client listens on that address. A public client is dialed back.
fn dialback(root: &Path) -> Value {
    let n = node(root, "n", Some("0.0.0.0"));
    let port = tcp_port(&n.listening()).expect("TCP listener");
    let at = |ip: &str| -> Multiaddr {
        format!("/ip4/{ip}/tcp/{port}/p2p/{}", n.id)
            .parse()
            .unwrap()
    };
    stage(
        root,
        "a dial-back to loopback is refused before any attempt",
    );
    let inside = NatPeer::start(Some("/ip4/127.0.0.1/tcp/0".parse().unwrap()), false);
    assert_eq!(inside.dial(at("127.0.0.1")), Ok(n.id));
    let refused = probed(&inside);
    thread::sleep(Duration::from_secs(1));
    let attempts = inside.connections(n.id);
    stage(root, "a dial-back to a public address is made");
    let outside = NatPeer::start(
        Some(format!("/ip4/{}/tcp/0", PUBLIC[0]).parse().unwrap()),
        false,
    );
    assert_eq!(outside.dial(at(PUBLIC[1])), Ok(n.id));
    let reached = probed(&outside);
    let loopback = refused
        .as_ref()
        .is_some_and(|(address, _)| address.to_string().starts_with("/ip4/127.0.0.1/"));
    json!({
        "loopback_tested":loopback,
        "loopback_reached":refused.is_some_and(|(_, ok)| ok),
        "connections":attempts,
        "public_reached":reached.is_some_and(|(_, ok)| ok),
    })
}

/// P's eight inbound data slots are taken, so two more peers arrive as
/// reservation holders: the one that reserves stays, the other is closed once
/// the grace period passes.
fn holders(root: &Path) -> Value {
    let p = forced(root, "p", PUBLIC[0], Reachability::Public);
    let fillers: Vec<NatPeer> = (0..8).map(|_| NatPeer::start(None, false)).collect();
    for filler in &fillers {
        assert_eq!(filler.dial(p.listening()), Ok(p.id));
    }
    stage(root, "two peers past the data budget, one reserving");
    let idle = NatPeer::start(None, false);
    let reserving = NatPeer::start(None, false);
    assert_eq!(idle.dial(p.listening()), Ok(p.id));
    assert_eq!(reserving.dial(p.listening()), Ok(p.id));
    reserving.listen(p.listening().with(Protocol::P2pCircuit));
    let reserved = until(Duration::from_secs(5), || {
        p.network.reserved_by_others() == vec![reserving.id]
    });
    let closed = until(Duration::from_secs(15), || idle.connections(p.id) == 0);
    json!({
        "reserved":reserved,
        "idle_closed":closed,
        "reserving_kept":reserving.connections(p.id) == 1,
        "fillers_kept":fillers.iter().all(|filler| filler.connections(p.id) == 1),
    })
}

/// B runs as the daemon does (service with inventory sync and discovery) with
/// only P as its seed, both on one selected chain, so sync asks P, hears
/// nothing new and releases it as idle. The release frees P's data slot but
/// keeps the connection that carries B's reservation.
fn daemon(root: &Path) -> Value {
    let p = forced_on(chained(root, "p"), PUBLIC[0], Reachability::Public);
    let b = forced_on(chained(root, "b"), PUBLIC[1], Reachability::Private);
    let mut book = PeerBook::empty(b.genesis());
    book.add(p.listening(), Origin::Seed).unwrap();
    let (discovery, _) = discover(root, "b", &b, book);
    let info = OverlayInfo {
        peer_id: b.id.to_string(),
        listen: Arc::new(Vec::new),
        announce: Vec::new(),
        peer_book: 1,
        connected: Arc::new(|| 0),
    };
    let sync = PeerSync {
        peers: b.network.sync_peers(Vec::new()),
        info,
    };
    let service = Service::bind(
        "127.0.0.1:0".parse().unwrap(),
        TOKEN.into(),
        b.engine.clone(),
    )
    .unwrap()
    .with_peer_sync(sync)
    .unwrap()
    .with_discovery(discovery)
    .unwrap();
    let handle = service.shutdown_handle();
    let worker = thread::spawn(move || service.run(true));
    stage(root, "the daemon reaches P and reserves on it");
    let reserved = until(Duration::from_secs(30), || b.network.relays() == vec![p.id]);
    stage(root, "idle sync releases P's data slot, not its connection");
    let demoted = until(Duration::from_secs(30), || {
        !b.network.has_direct(p.id) && b.connected().contains(&p.id)
    });
    stage(root, "the reservation holds across sync steps");
    let held = (0..60).all(|_| {
        thread::sleep(Duration::from_millis(500));
        b.network.relays() == vec![p.id] && b.connected().contains(&p.id)
    });
    handle.request();
    assert!(worker.join().unwrap().is_ok());
    json!({"reserved":reserved,"demoted":demoted,"held":held})
}

/// P relays for B; returns B's circuit address through P.
fn reserved(root: &Path, p: &Node, b: &Node) -> Multiaddr {
    stage(root, "a private node reserves on the public peer it dialed");
    assert_eq!(b.dial(p.listening()), p.id);
    let held = until(Duration::from_secs(20), || {
        b.network.relays() == vec![p.id]
            && b.network.circuit_addresses().len() == 1
            && p.network.reserved_by_others() == vec![b.id]
    });
    assert!(
        held,
        "{:?} {:?} {:?} {:?}",
        b.network.relays(),
        b.network.circuit_addresses(),
        b.connected(),
        p.network.reserved_by_others()
    );
    b.network.circuit_addresses().remove(0)
}

fn relay(root: &Path) -> Value {
    let p = forced(root, "p", PUBLIC[0], Reachability::Public);
    let b = forced(root, "b", PUBLIC[1], Reachability::Private);
    let circuit = reserved(root, &p, &b);
    stage(root, "the reservation outlives the data idle timeout");
    thread::sleep(IDLE);
    let survived = b.network.relays() == vec![p.id] && b.connected().contains(&p.id);
    stage(root, "records are refused on a relayed connection");
    let a = NatPeer::start(None, false);
    assert_eq!(a.dial(circuit), Ok(b.id));
    let relayed = until(Duration::from_secs(5), || {
        link(&b, a.id) == Some(PeerLink::Relayed)
    });
    let refused = a.call(b.id, Kind::Peers { limit: 0 }).is_err();
    let direct = b.network.has_direct(a.id);
    stage(
        root,
        "a direct link beside the circuit carries records and retires it",
    );
    assert_eq!(a.dial(b.listening()), Ok(b.id));
    let deadline = Instant::now() + Duration::from_secs(10);
    let asked = b
        .network
        .peers_of(a.id, 0, &AtomicBool::new(false), deadline);
    let retired = until(Duration::from_secs(5), || a.connections(b.id) == 1);
    let served = matches!(a.call(b.id, Kind::Peers { limit: 0 }), Ok(Reply::Peers(_)));
    json!({
        "survived":survived,"relayed":relayed,"refused":refused,"direct":direct,
        "asked":asked.is_ok(),"retired":retired,"served":served,
        "link":link(&b, a.id).map(|l| l.as_str()),
    })
}

/// A and B listen on PUBLIC[0] and reach P on PUBLIC[1] from that address, so
/// P observes each at its own listener and the punch never depends on a
/// simultaneous open.
fn punch(root: &Path) -> Value {
    let p = forced(root, "p", PUBLIC[1], Reachability::Public);
    let b = forced(root, "b", PUBLIC[0], Reachability::Private);
    let circuit = reserved(root, &p, &b);
    stage(
        root,
        "A learns its address from P, then reaches B through P",
    );
    let a = NatPeer::start(
        Some(format!("/ip4/{}/tcp/0", PUBLIC[0]).parse().unwrap()),
        true,
    );
    assert_eq!(a.dial(p.listening()), Ok(p.id));
    thread::sleep(Duration::from_secs(3));
    assert_eq!(a.dial(circuit), Ok(b.id));
    stage(root, "DCUtR replaces the circuit with a direct data link");
    let direct = until(PROBE, || {
        b.network.has_direct(a.id) && link(&b, a.id) == Some(PeerLink::Direct)
    });
    let served = until(Duration::from_secs(10), || {
        matches!(a.call(b.id, Kind::Peers { limit: 0 }), Ok(Reply::Peers(_)))
    });
    json!({"direct":direct,"served":served})
}

#[test]
#[ignore = "owned namespaced subprocess host, invoked only by the NAT cases"]
fn peer_nat_host() {
    let (root, scenario) = host();
    let checks = match scenario.as_str() {
        "quic" => quic(&root),
        "autonat" => autonat(&root),
        "relay" => relay(&root),
        "punch" => punch(&root),
        "dialback" => dialback(&root),
        "holders" => holders(&root),
        "daemon" => daemon(&root),
        _ => panic!("unknown owned scenario"),
    };
    report(&root, &scenario, checks);
}

#[test]
fn a_daemon_keeps_its_reservation_across_sync_steps() {
    let checks = run(HOST, "daemon");
    assert_eq!(checks["reserved"], true, "{checks}");
    assert_eq!(checks["demoted"], true, "{checks}");
    assert_eq!(checks["held"], true, "{checks}");
}

#[test]
fn a_reservation_holder_that_never_reserves_is_closed() {
    let checks = run(HOST, "holders");
    assert_eq!(checks["reserved"], true, "{checks}");
    assert_eq!(checks["idle_closed"], true, "{checks}");
    assert_eq!(checks["reserving_kept"], true, "{checks}");
    assert_eq!(checks["fillers_kept"], true, "{checks}");
}

#[test]
fn autonat_dial_backs_reach_only_public_ipv4() {
    let checks = run(HOST, "dialback");
    assert_eq!(checks["loopback_tested"], true);
    assert_eq!(checks["loopback_reached"], false);
    assert_eq!(checks["connections"], 1, "no dial-back connection was made");
    assert_eq!(checks["public_reached"], true);
}

#[test]
fn quic_listens_on_the_tcp_port_and_serves_records() {
    let checks = run(HOST, "quic");
    assert_eq!(checks["same_port"], true);
    assert_eq!(checks["served"], true);
    assert_eq!(checks["link"], "direct");
}

#[test]
fn autonat_confirms_dialable_addresses_and_never_advertises_observations() {
    let checks = run(HOST, "autonat");
    assert_eq!(checks["public"], true, "{checks}");
    assert_eq!(checks["external"], true);
    assert_eq!(checks["private"], true);
    assert_eq!(checks["hidden_external"], 0);
}

#[test]
fn a_reservation_outlives_idle_data_and_relayed_links_refuse_records() {
    let checks = run(HOST, "relay");
    assert_eq!(checks["survived"], true);
    assert_eq!(checks["relayed"], true);
    assert_eq!(checks["refused"], true);
    assert_eq!(checks["direct"], false);
    assert_eq!(
        checks["asked"], true,
        "a request beside a circuit is never refused"
    );
    assert_eq!(checks["retired"], true);
    assert_eq!(checks["served"], true);
    assert_eq!(checks["link"], "direct");
}

#[test]
fn dcutr_upgrades_a_relayed_contact_to_a_direct_data_link() {
    let checks = run(HOST, "punch");
    assert_eq!(checks["direct"], true);
    assert_eq!(checks["served"], true);
}
