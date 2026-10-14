//! Circuit addresses in the peer book (contract §3.5): learned only from
//! gossip, always naming their peer, stored without the target, bounded by
//! their own share of the book and dialed after direct addresses. No sockets.
use bitcoin::{BlockHash, hashes::Hash};
use libp2p::{Multiaddr, PeerId, multiaddr::Protocol};
use serde_json::json;
use urma_node::{
    peer_protocol::GossipPeer,
    peerbook::{Origin, PeerBook},
};

const NOW: u64 = 1_800_000_000;
const LOCAL: u16 = 999;
const RELAY: u16 = 998;

fn genesis() -> BlockHash {
    BlockHash::from_byte_array([42; 32])
}

// Synthetic SHA-256 peer IDs are routing fixtures, not claims about keys.
fn peer(index: u16) -> PeerId {
    let mut bytes = [0_u8; 34];
    bytes[0] = 0x12;
    bytes[1] = 32;
    bytes[2..4].copy_from_slice(&index.to_be_bytes());
    PeerId::from_bytes(&bytes).unwrap()
}

fn bare(text: &str) -> Multiaddr {
    text.parse().unwrap()
}

/// A circuit through a public relay, without the target.
fn through(relay: PeerId) -> Multiaddr {
    bare("/ip4/11.0.0.1/tcp/28332")
        .with(Protocol::P2p(relay))
        .with(Protocol::P2pCircuit)
}

fn circuit(target: PeerId) -> Multiaddr {
    through(peer(RELAY)).with(Protocol::P2p(target))
}

/// One public IPv4 per index, each in its own /24.
fn spread(index: u16) -> Multiaddr {
    let [high, low] = index.to_be_bytes();
    bare(&format!("/ip4/12.{high}.{low}.1/tcp/28332"))
}

fn gossip(id: PeerId, addrs: Vec<Multiaddr>) -> GossipPeer {
    GossipPeer {
        peer_id: id,
        addrs,
        last_seen: NOW,
    }
}

fn merge(book: &mut PeerBook, peers: &[GossipPeer]) -> usize {
    book.merge(peers, peer(LOCAL), &[])
}

fn circuit_only(book: &PeerBook) -> usize {
    book.entries()
        .iter()
        .filter(|entry| entry.circuit_only())
        .count()
}

#[test]
fn circuits_come_only_from_gossip_and_name_their_peer() {
    let mut book = PeerBook::empty(genesis());
    assert!(book.add(circuit(peer(1)), Origin::Manual).is_err());
    assert!(book.add(circuit(peer(1)), Origin::Seed).is_err());
    assert!(book.add(through(peer(RELAY)), Origin::Gossip).is_err());
    let private = bare("/ip4/10.0.0.1/tcp/28332")
        .with(Protocol::P2p(peer(RELAY)))
        .with(Protocol::P2pCircuit)
        .with(Protocol::P2p(peer(1)));
    assert!(book.add(private, Origin::Gossip).is_err());
    assert!(book.entries().is_empty());
    assert!(book.add(circuit(peer(1)), Origin::Gossip).unwrap());
    let entry = &book.entries()[0];
    assert_eq!(entry.peer_id, Some(peer(1)));
    assert_eq!(entry.addrs, vec![through(peer(RELAY))]);
    assert!(entry.circuit_only());
}

#[test]
fn circuit_only_entries_keep_to_their_share_and_evict_among_themselves() {
    let mut book = PeerBook::empty(genesis());
    let direct: Vec<GossipPeer> = (1..=8)
        .map(|index| gossip(peer(index), vec![spread(index)]))
        .collect();
    assert_eq!(merge(&mut book, &direct), 8);
    let relayed: Vec<GossipPeer> = (100..400)
        .map(|index| gossip(peer(index), vec![through(peer(RELAY))]))
        .collect();
    merge(&mut book, &relayed);
    assert_eq!(circuit_only(&book), PeerBook::CIRCUIT_CAPACITY);
    assert_eq!(book.entries().len(), PeerBook::CIRCUIT_CAPACITY + 8);
    for index in 1..=8 {
        let kept = book
            .entries()
            .iter()
            .any(|entry| entry.peer_id == Some(peer(index)));
        assert!(kept, "direct entry {index} evicted by circuits");
    }
    let newest = book
        .entries()
        .iter()
        .any(|entry| entry.peer_id == Some(peer(399)));
    assert!(newest, "a fresh circuit entry makes room among circuits");
}

#[test]
fn a_crowded_circuit_share_keeps_reached_circuit_entries() {
    let mut book = PeerBook::empty(genesis());
    for index in 100..356 {
        book.add(circuit(peer(index)), Origin::Gossip).unwrap();
        book.succeeded(&circuit(peer(index)), peer(index), peer(LOCAL), NOW);
    }
    assert!(book.add(circuit(peer(500)), Origin::Gossip).is_err());
    assert_eq!(circuit_only(&book), PeerBook::CIRCUIT_CAPACITY);
}

#[test]
fn a_saved_book_over_the_circuit_share_loads_without_its_worst_circuits() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("peers.json");
    let entries: Vec<_> = (100..400_u16)
        .map(|index| {
            let failed = index < 144;
            json!({"peer_id": peer(index).to_string(),
                "addrs": [through(peer(RELAY)).to_string()], "origin": "gossip",
                "last_success": (!failed).then_some(NOW), "failures": if failed { 3 } else { 0 },
                "banned_until": null})
        })
        .collect();
    let document =
        json!({"version": 2, "chain_genesis": genesis().to_string(), "entries": entries});
    std::fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
    let book = PeerBook::load(&path, genesis()).unwrap();
    assert_eq!(circuit_only(&book), PeerBook::CIRCUIT_CAPACITY);
    assert!(book.entries().iter().all(|entry| entry.failures == 0));
}

#[test]
fn a_failed_address_moves_back_so_the_next_dial_tries_another() {
    let mut book = PeerBook::empty(genesis());
    let first = spread(1).with(Protocol::P2p(peer(1)));
    let second = spread(2).with(Protocol::P2p(peer(1)));
    book.add(first.clone(), Origin::Manual).unwrap();
    book.add(second.clone(), Origin::Manual).unwrap();
    assert_eq!(book.candidates(NOW, &[], &[], 1), vec![first.clone()]);
    book.failed(&first, NOW);
    assert_eq!(book.candidates(NOW + 61, &[], &[], 1), vec![second.clone()]);
    book.failed(&second, NOW + 61);
    assert_eq!(book.candidates(NOW + 200, &[], &[], 1), vec![first]);
}

#[test]
fn circuits_skip_the_network_group_cap() {
    let mut book = PeerBook::empty(genesis());
    for index in 1..=8 {
        assert!(book.add(circuit(peer(index)), Origin::Gossip).unwrap());
    }
    assert_eq!(circuit_only(&book), 8);
}

#[test]
fn direct_entries_are_dialed_before_circuit_only_ones() {
    let mut book = PeerBook::empty(genesis());
    merge(&mut book, &[gossip(peer(1), vec![through(peer(RELAY))])]);
    merge(&mut book, &[gossip(peer(2), vec![spread(2)])]);
    merge(
        &mut book,
        &[gossip(peer(3), vec![through(peer(RELAY)), spread(3)])],
    );
    let picked = book.candidates(NOW, &[], &[], 3);
    assert_eq!(picked.len(), 3);
    assert_eq!(picked[2], circuit(peer(1)), "circuit-only last: {picked:?}");
    let mixed = spread(3).with(Protocol::P2p(peer(3)));
    assert!(picked[..2].contains(&mixed), "direct address preferred");
    let one = book.candidates(NOW, &[peer(2), peer(3)], &[], 1);
    assert_eq!(one, vec![circuit(peer(1))]);
}

#[test]
fn gossip_drops_circuits_through_the_peer_itself_or_through_us() {
    let mut book = PeerBook::empty(genesis());
    let itself = gossip(peer(1), vec![through(peer(1))]);
    let via_us = gossip(peer(2), vec![through(peer(LOCAL))]);
    assert_eq!(merge(&mut book, &[itself, via_us]), 0);
    assert!(book.entries().is_empty());
    let other = gossip(peer(3), vec![circuit(peer(4))]);
    assert_eq!(
        merge(&mut book, &[other]),
        0,
        "a circuit naming another peer"
    );
}

#[test]
fn reached_circuit_entries_are_served_but_never_become_sync_sources() {
    let mut book = PeerBook::empty(genesis());
    book.add(circuit(peer(1)), Origin::Gossip).unwrap();
    book.succeeded(&circuit(peer(1)), peer(1), peer(LOCAL), NOW);
    let served = book.gossip(8, &[]);
    assert_eq!(served.len(), 1);
    assert_eq!(served[0].addrs, vec![through(peer(RELAY))]);
    assert!(book.sync_addresses(&[]).is_empty());
}
