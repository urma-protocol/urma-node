//! NAT state the network thread maintains and the owner reads: reachability
//! from AutoNAT, the external addresses it confirmed, relay reservations held
//! by this node and on it, the circuit addresses it listens on, and hole
//! punching counters. A relayed connection only reaches a peer and hole
//! punches; data never rides it.
use crate::{peer_gossip::guard, recorded::RecordedError};
use libp2p::{Multiaddr, PeerId, multiaddr::Protocol};
use std::{collections::BTreeSet, fmt::Display, sync::Mutex};

/// How a connection reaches its peer: over its own transport, or through a
/// relay's circuit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeerLink {
    Direct,
    Relayed,
}

impl PeerLink {
    /// A connection whose remote address passes through `/p2p-circuit`.
    pub fn of(remote: &Multiaddr) -> Self {
        match remote.iter().any(|part| part == Protocol::P2pCircuit) {
            true => Self::Relayed,
            false => Self::Direct,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Relayed => "relayed",
        }
    }
}

/// Whether peers outside can dial this node, as AutoNAT last concluded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Reachability {
    #[default]
    Unknown,
    Public,
    Private,
}

impl Reachability {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Public => "public",
            Self::Private => "private",
        }
    }
}

/// DCUtR counters since start, with the latest failure.
#[derive(Clone, Debug, Default)]
pub(crate) struct HolePunch {
    pub(crate) attempts: u64,
    pub(crate) successes: u64,
    pub(crate) last_error: Option<RecordedError>,
}

#[derive(Default)]
pub(crate) struct Nat {
    reachability: Mutex<Reachability>,
    external: Mutex<Vec<Multiaddr>>,
    relays: Mutex<BTreeSet<PeerId>>,
    reserved_by_others: Mutex<BTreeSet<PeerId>>,
    circuits: Mutex<Vec<Multiaddr>>,
    punch: Mutex<HolePunch>,
}

fn mark(set: &Mutex<BTreeSet<PeerId>>, peer: PeerId, active: bool) {
    let mut set = guard(set);
    match active {
        true => set.insert(peer),
        false => set.remove(&peer),
    };
}

fn place(list: &Mutex<Vec<Multiaddr>>, address: Multiaddr, present: bool) {
    let mut list = guard(list);
    list.retain(|known| *known != address);
    if present {
        list.push(address);
    }
}

impl Nat {
    pub(crate) fn reachability(&self) -> Reachability {
        *guard(&self.reachability)
    }

    pub(crate) fn set_reachability(&self, reachability: Reachability) {
        *guard(&self.reachability) = reachability;
    }

    /// Addresses AutoNAT confirmed or the operator announced, as advertised,
    /// without `/p2p`.
    pub(crate) fn external(&self) -> Vec<Multiaddr> {
        guard(&self.external).clone()
    }

    pub(crate) fn set_external(&self, address: Multiaddr, confirmed: bool) {
        place(&self.external, address, confirmed);
    }

    /// Relays holding an accepted reservation for this node.
    pub(crate) fn relays(&self) -> Vec<PeerId> {
        guard(&self.relays).iter().copied().collect()
    }

    pub(crate) fn set_relay(&self, relay: PeerId, active: bool) {
        mark(&self.relays, relay, active);
    }

    /// Peers holding an active reservation on this node's relay server.
    pub(crate) fn reserved_by_others(&self) -> Vec<PeerId> {
        guard(&self.reserved_by_others).iter().copied().collect()
    }

    pub(crate) fn set_reserved_by(&self, holder: PeerId, active: bool) {
        mark(&self.reserved_by_others, holder, active);
    }

    /// `/p2p-circuit` addresses this node listens on through its relays.
    pub(crate) fn circuits(&self) -> Vec<Multiaddr> {
        guard(&self.circuits).clone()
    }

    pub(crate) fn set_circuit(&self, address: Multiaddr, bound: bool) {
        place(&self.circuits, address, bound);
    }

    pub(crate) fn hole_punch(&self) -> HolePunch {
        guard(&self.punch).clone()
    }

    pub(crate) fn punch_succeeded(&self) {
        let mut punch = guard(&self.punch);
        punch.attempts += 1;
        punch.successes += 1;
    }

    pub(crate) fn punch_failed(&self, error: impl Display) {
        let mut punch = guard(&self.punch);
        punch.attempts += 1;
        punch.last_error = Some(RecordedError::now(error));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        peer_gossip::Shared, peer_limits::Class, peer_protocol::Reply, peerbook::PeerBook,
    };
    use bitcoin::{BlockHash, hashes::Hash};
    use libp2p::identity::Keypair;

    fn peer() -> PeerId {
        Keypair::generate_ed25519().public().to_peer_id()
    }

    #[test]
    fn a_circuit_remote_is_relayed_and_any_direct_connection_makes_the_peer_direct() {
        let relay = peer();
        let circuit: Multiaddr = format!("/ip4/1.2.3.4/tcp/1/p2p/{relay}/p2p-circuit")
            .parse()
            .unwrap();
        assert_eq!(PeerLink::of(&circuit), PeerLink::Relayed);
        assert_eq!(
            PeerLink::of(&"/ip4/1.2.3.4/udp/1/quic-v1".parse().unwrap()),
            PeerLink::Direct
        );
        let shared = Shared::new(peer());
        let remote = peer();
        shared.opened(remote, false, Class::Relayed);
        assert!(!shared.has_direct(remote));
        assert_eq!(shared.links(), vec![(remote, PeerLink::Relayed)]);
        shared.opened(remote, false, Class::Auxiliary);
        assert!(
            !shared.has_direct(remote),
            "only a data connection carries data"
        );
        assert_eq!(shared.links(), vec![(remote, PeerLink::Direct)]);
        shared.closed(remote, false, Class::Auxiliary);
        shared.opened(remote, true, Class::Data);
        assert!(shared.has_direct(remote));
        shared.closed(remote, false, Class::Relayed);
        assert_eq!(shared.links(), vec![(remote, PeerLink::Direct)]);
        shared.closed(remote, true, Class::Data);
        assert!(shared.links().is_empty());
    }

    #[test]
    fn a_relayed_only_peer_is_neither_live_nor_outbound_until_it_has_a_direct_link() {
        let shared = Shared::new(peer());
        let remote = peer();
        shared.opened(remote, true, Class::Relayed);
        shared.on_chain(remote);
        assert!(shared.is_connected(remote));
        assert!(shared.live().is_empty());
        assert!(!shared.is_outbound(remote));
        shared.opened(remote, true, Class::Data);
        assert_eq!(shared.live(), vec![remote]);
        assert!(shared.is_outbound(remote));
    }

    #[test]
    fn a_relay_serves_its_reservation_holders_as_circuits_a_book_stores() {
        let relay = peer();
        let shared = Shared::new(relay);
        let (holder, requester) = (peer(), peer());
        shared.nat.set_reserved_by(holder, true);
        shared.nat.set_reserved_by(requester, true);
        assert!(
            matches!(shared.serve(requester, false, 8), Reply::Peers(peers) if peers.is_empty())
        );
        let external: Multiaddr = "/ip4/11.0.0.1/tcp/28332".parse().unwrap();
        shared.nat.set_external(external.clone(), true);
        let Reply::Peers(peers) = shared.serve(requester, false, 8) else {
            panic!("peers request refused");
        };
        let hop = external
            .with(Protocol::P2p(relay))
            .with(Protocol::P2pCircuit);
        assert_eq!(peers.len(), 1, "the requester is never listed");
        assert_eq!(
            (peers[0].peer_id, peers[0].addrs.clone()),
            (holder, vec![hop.clone()])
        );
        let mut book = PeerBook::empty(BlockHash::from_byte_array([42; 32]));
        assert_eq!(book.merge(&peers, requester, &[]), 1);
        let entry = &book.entries()[0];
        assert_eq!(
            (entry.peer_id, entry.addrs.clone()),
            (Some(holder), vec![hop])
        );
        assert!(entry.circuit_only());
    }

    #[test]
    fn reservations_circuits_and_punch_counters_track_their_latest_state() {
        let nat = Nat::default();
        let (relay, holder) = (peer(), peer());
        nat.set_relay(relay, true);
        nat.set_reserved_by(holder, true);
        assert_eq!(
            (nat.relays(), nat.reserved_by_others()),
            (vec![relay], vec![holder])
        );
        nat.set_reserved_by(holder, false);
        assert!(nat.reserved_by_others().is_empty());
        let circuit: Multiaddr = format!("/ip4/1.2.3.4/tcp/1/p2p/{relay}/p2p-circuit")
            .parse()
            .unwrap();
        nat.set_circuit(circuit.clone(), true);
        nat.set_circuit(circuit.clone(), true);
        assert_eq!(nat.circuits(), vec![circuit]);
        nat.punch_succeeded();
        nat.punch_failed("timeout");
        let punch = nat.hole_punch();
        assert_eq!((punch.attempts, punch.successes), (2, 1));
        assert_eq!(
            punch.last_error.map(|error| error.message).as_deref(),
            Some("timeout")
        );
        assert_eq!(nat.reachability(), Reachability::Unknown);
    }
}
