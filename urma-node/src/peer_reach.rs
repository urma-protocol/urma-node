//! NAT handling for the network loop (URMA_NODE_NAT.md §3.2–§3.6a):
//! reachability from AutoNAT, the external-address policy, the relay
//! reservation manager, relay server bookkeeping, hole punch outcomes, and the
//! QUIC listener on the TCP listener's port.
use crate::{
    Error,
    peer_gossip::{is_loopback, is_public},
    peer_limits::{Class, Classes},
    peer_nat::{Nat, PeerLink, Reachability},
    peer_transport::{Behaviour, BehaviourEvent},
};
use libp2p::{
    Multiaddr, PeerId, Swarm, autonat,
    core::{ConnectedPoint, transport::ListenerId},
    dcutr,
    multiaddr::Protocol,
    relay,
    swarm::{ConnectionId, SwarmEvent},
};
use std::{
    collections::HashMap,
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

/// One live connection as the loop admitted it.
pub(crate) struct Remote {
    pub(crate) peer: PeerId,
    pub(crate) address: Multiaddr,
    pub(crate) outbound: bool,
    pub(crate) class: Class,
}

/// Live connections by id. The class is kept here because admission forgets
/// it before the loop sees the connection close.
#[derive(Default)]
pub(crate) struct Remotes(HashMap<ConnectionId, Remote>);

impl Remotes {
    /// Records an established connection with the class admission gave it.
    pub(crate) fn admit(
        &mut self,
        (id, peer): (ConnectionId, PeerId),
        endpoint: &ConnectedPoint,
        classes: &Classes,
    ) -> Class {
        let class = match classes.of(id) {
            Some(class) => class,
            None => {
                tracing::warn!(%peer, "connection without an admission class; kept off data");
                Class::Auxiliary
            }
        };
        let remote = Remote {
            peer,
            address: endpoint.get_remote_address().clone(),
            outbound: endpoint.is_dialer(),
            class,
        };
        self.0.insert(id, remote);
        class
    }

    pub(crate) fn remove(&mut self, id: ConnectionId) -> Option<Remote> {
        self.0.remove(&id)
    }

    pub(crate) fn loopback(&self, id: ConnectionId) -> bool {
        self.0
            .get(&id)
            .is_some_and(|remote| is_loopback(&remote.address))
    }

    /// Only peers we dialed directly may witness our address (§3.6a.4).
    pub(crate) fn outbound_direct(&self, id: ConnectionId) -> bool {
        self.0
            .get(&id)
            .is_some_and(|remote| remote.outbound && remote.class != Class::Relayed)
    }

    /// A relay circuit has no use once the peer holds a direct data link
    /// (§3.4): the circuits to retire after `id` was admitted as `class`.
    pub(crate) fn superseded(
        &self,
        id: ConnectionId,
        peer: PeerId,
        class: Class,
    ) -> Vec<ConnectionId> {
        let direct = self
            .0
            .values()
            .any(|remote| remote.peer == peer && remote.class == Class::Data);
        match class {
            Class::Data => self.relayed_of(peer),
            Class::Relayed if direct => vec![id],
            Class::Relayed | Class::Auxiliary | Class::Holder => Vec::new(),
        }
    }

    fn relayed_of(&self, peer: PeerId) -> Vec<ConnectionId> {
        self.0
            .iter()
            .filter(|(_, remote)| remote.peer == peer && remote.class == Class::Relayed)
            .map(|(id, _)| *id)
            .collect()
    }

    /// Public peers we hold an outbound data connection to, oldest first, with
    /// the address we reached them on (without `/p2p`).
    fn relay_candidates(&self) -> Vec<(PeerId, Multiaddr)> {
        let mut candidates: Vec<(&ConnectionId, &Remote)> = self
            .0
            .iter()
            .filter(|(_, remote)| {
                remote.outbound && remote.class == Class::Data && is_public(&remote.address)
            })
            .collect();
        candidates.sort_by_key(|(id, _)| **id);
        candidates
            .into_iter()
            .map(|(_, remote)| (remote.peer, without_peer(&remote.address)))
            .collect()
    }
}

pub(crate) fn without_peer(address: &Multiaddr) -> Multiaddr {
    address
        .iter()
        .filter(|part| !matches!(part, Protocol::P2p(_)))
        .collect()
}

/// `/ip4|ip6/<public literal>/tcp/<port>` or `/udp/<port>/quic-v1`: the shapes
/// this node advertises or offers as its own address.
pub(crate) fn direct_shape(address: &Multiaddr) -> bool {
    let mut parts = address.iter();
    let ip = matches!(parts.next(), Some(Protocol::Ip4(_) | Protocol::Ip6(_)));
    let transport = match (parts.next(), parts.next()) {
        (Some(Protocol::Tcp(port)), None) => port != 0,
        (Some(Protocol::Udp(port)), Some(Protocol::QuicV1)) => port != 0 && parts.next().is_none(),
        _ => false,
    };
    ip && transport && is_public(address)
}

/// An operator announce address: a public IP literal with TCP or UDP/QUIC v1,
/// ending at most in this node's own /p2p. Returned without /p2p, as identify
/// advertises it.
pub(crate) fn validate_announce(address: &Multiaddr, local: PeerId) -> Result<Multiaddr, Error> {
    let mut bare = address.clone();
    if let Some(Protocol::P2p(named)) = address.iter().last() {
        if named != local {
            return Err(Error::Invalid("announce address names another peer".into()));
        }
        bare.pop();
    }
    if !is_public(&bare) {
        return Err(Error::Invalid("announce address must be public".into()));
    }
    if !direct_shape(&bare) {
        return Err(Error::Invalid(
            "announce address must be an IP literal with TCP or UDP/QUIC v1".into(),
        ));
    }
    Ok(bare)
}

/// How the node decides its reachability: from AutoNAT probes, or fixed (a
/// test stand-in for what AutoNAT would conclude).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReachMode {
    AutoNat,
    Forced(Reachability),
}

pub(crate) struct Reach {
    forced: ReachMode,
    announced: bool,
    probe_failed: bool,
    relay_open: Arc<AtomicBool>,
    local: PeerId,
    listen: Option<Multiaddr>,
    quic: bool,
    reservations: HashMap<ListenerId, PeerId>,
    backoff: HashMap<PeerId, Instant>,
}

impl Reach {
    const RESERVATIONS: usize = 2;
    const BACKOFF: Duration = Duration::from_secs(10 * 60);

    pub(crate) fn new(
        forced: ReachMode,
        announced: bool,
        relay_open: Arc<AtomicBool>,
        (local, listen): (PeerId, Option<Multiaddr>),
    ) -> Self {
        Self {
            forced,
            announced,
            probe_failed: false,
            relay_open,
            local,
            listen,
            quic: false,
            reservations: HashMap::new(),
            backoff: HashMap::new(),
        }
    }

    /// Public with an announced or AutoNAT-confirmed direct address, private
    /// once a probe failed without one, unknown before that; forced in tests.
    pub(crate) fn update(&self, nat: &Nat) {
        let observed = match (
            self.announced || !nat.external().is_empty(),
            self.probe_failed,
        ) {
            (true, _) => Reachability::Public,
            (false, true) => Reachability::Private,
            (false, false) => Reachability::Unknown,
        };
        let reachability = match self.forced {
            ReachMode::Forced(reachability) => reachability,
            ReachMode::AutoNat => observed,
        };
        nat.set_reachability(reachability);
        self.relay_open
            .store(reachability == Reachability::Public, Ordering::Release);
    }

    /// Takes the loop's NAT events and hands every other event back.
    pub(crate) fn absorb(
        &mut self,
        swarm: &mut Swarm<Behaviour>,
        remotes: &Remotes,
        nat: &Nat,
        event: SwarmEvent<BehaviourEvent>,
    ) -> Option<SwarmEvent<BehaviourEvent>> {
        match event {
            SwarmEvent::Behaviour(BehaviourEvent::Autonat(event)) => self.autonat(&event, nat),
            SwarmEvent::Behaviour(BehaviourEvent::AutonatServer(event)) => {
                tracing::debug!(?event, "AutoNAT probe served")
            }
            SwarmEvent::Behaviour(BehaviourEvent::Relay(event)) => Self::relay_server(&event, nat),
            SwarmEvent::Behaviour(BehaviourEvent::RelayClient(event)) => {
                Self::relay_client(&event, nat)
            }
            SwarmEvent::Behaviour(BehaviourEvent::Dcutr(event)) => {
                Self::dcutr(swarm, remotes, event, nat)
            }
            SwarmEvent::ExternalAddrConfirmed { address } => self.external(address, true, nat),
            SwarmEvent::ExternalAddrExpired { address } => self.external(address, false, nat),
            SwarmEvent::NewListenAddr { ref address, .. }
                if self.listen_addr(swarm, address, nat) => {}
            SwarmEvent::ExpiredListenAddr { ref address, .. }
                if self.listen_expired(address, nat) => {}
            SwarmEvent::ListenerClosed {
                listener_id,
                ref addresses,
                ref reason,
            } => {
                for address in addresses {
                    self.listen_expired(address, nat);
                }
                self.listener_closed(listener_id, reason, nat);
                return Some(event);
            }
            other => return Some(other),
        }
        None
    }

    pub(crate) fn autonat(&mut self, event: &autonat::v2::client::Event, nat: &Nat) {
        if event.result.is_err() {
            self.probe_failed = true;
        }
        self.update(nat);
    }

    /// Circuit addresses the relay client confirms stay out of the direct set.
    pub(crate) fn external(&self, address: Multiaddr, confirmed: bool, nat: &Nat) {
        if PeerLink::of(&address) == PeerLink::Direct {
            nat.set_external(address, confirmed);
            self.update(nat);
        }
    }

    pub(crate) fn relay_server(event: &relay::Event, nat: &Nat) {
        match event {
            relay::Event::ReservationReqAccepted { src_peer_id, .. } => {
                nat.set_reserved_by(*src_peer_id, true)
            }
            relay::Event::ReservationClosed { src_peer_id }
            | relay::Event::ReservationTimedOut { src_peer_id } => {
                nat.set_reserved_by(*src_peer_id, false)
            }
            _ => (),
        }
    }

    pub(crate) fn relay_client(event: &relay::client::Event, nat: &Nat) {
        if let relay::client::Event::ReservationReqAccepted { relay_peer_id, .. } = event {
            nat.set_relay(*relay_peer_id, true);
        }
    }

    /// A punched direct link retires the circuit; a failure is recorded and
    /// the next relayed contact tries again.
    pub(crate) fn dcutr(
        swarm: &mut Swarm<Behaviour>,
        remotes: &Remotes,
        event: dcutr::Event,
        nat: &Nat,
    ) {
        match event.result {
            Ok(_direct) => {
                nat.punch_succeeded();
                for id in remotes.relayed_of(event.remote_peer_id) {
                    swarm.close_connection(id);
                }
            }
            Err(error) => nat.punch_failed(format!("{}: {error}", event.remote_peer_id)),
        }
    }

    /// Returns true when the address is a relay circuit, kept out of the
    /// direct listeners; the first TCP listener address opens QUIC beside it.
    pub(crate) fn listen_addr(
        &mut self,
        swarm: &mut Swarm<Behaviour>,
        address: &Multiaddr,
        nat: &Nat,
    ) -> bool {
        if PeerLink::of(address) == PeerLink::Relayed {
            nat.set_circuit(self.with_local(address), true);
            return true;
        }
        self.open_quic(swarm, address);
        false
    }

    pub(crate) fn listen_expired(&self, address: &Multiaddr, nat: &Nat) -> bool {
        let circuit = PeerLink::of(address) == PeerLink::Relayed;
        if circuit {
            nat.set_circuit(self.with_local(address), false);
        }
        circuit
    }

    pub(crate) fn listener_closed(
        &mut self,
        id: ListenerId,
        reason: &Result<(), io::Error>,
        nat: &Nat,
    ) {
        let Some(relay) = self.reservations.remove(&id) else {
            return;
        };
        nat.set_relay(relay, false);
        if reason.is_err() {
            self.backoff.insert(relay, Instant::now() + Self::BACKOFF);
        }
    }

    fn with_local(&self, address: &Multiaddr) -> Multiaddr {
        match address.iter().last() {
            Some(Protocol::P2p(_)) => address.clone(),
            _ => address.clone().with(Protocol::P2p(self.local)),
        }
    }

    fn open_quic(&mut self, swarm: &mut Swarm<Behaviour>, bound: &Multiaddr) {
        let Some(listen) = &self.listen else {
            return;
        };
        let port = bound.iter().find_map(|part| match part {
            Protocol::Tcp(port) => Some(port),
            _ => None,
        });
        let (Some(port), Some(ip), false) = (port, listen.iter().next(), self.quic) else {
            return;
        };
        self.quic = true;
        let quic = Multiaddr::empty()
            .with(ip)
            .with(Protocol::Udp(port))
            .with(Protocol::QuicV1);
        if let Err(error) = swarm.listen_on(quic.clone()) {
            tracing::warn!(%error, %quic, "QUIC listener unavailable; TCP only");
        }
    }

    /// While private, holds up to two reservations on public peers we dialed
    /// (seeds first), skipping relays that recently failed; otherwise none.
    pub(crate) fn maintain(&mut self, swarm: &mut Swarm<Behaviour>, remotes: &Remotes, nat: &Nat) {
        if nat.reachability() != Reachability::Private {
            for (id, _) in self.reservations.drain() {
                swarm.remove_listener(id);
            }
            return;
        }
        let now = Instant::now();
        self.backoff.retain(|_, until| *until > now);
        for (relay, address) in remotes.relay_candidates() {
            if self.reservations.len() >= Self::RESERVATIONS {
                return;
            }
            let held = self.reservations.values().any(|known| *known == relay);
            if held || relay == self.local || self.backoff.contains_key(&relay) {
                continue;
            }
            let circuit = address
                .with(Protocol::P2p(relay))
                .with(Protocol::P2pCircuit);
            match swarm.listen_on(circuit) {
                Ok(id) => {
                    self.reservations.insert(id, relay);
                }
                Err(error) => {
                    tracing::warn!(%error, %relay, "relay reservation not attempted");
                    self.backoff.insert(relay, now + Self::BACKOFF);
                }
            }
        }
    }
}
