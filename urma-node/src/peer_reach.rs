//! NAT handling for the network loop (URMA_NODE_NAT.md §3.2–§3.6a):
//! reachability from AutoNAT, the external-address policy, the relay
//! reservation manager, relay server bookkeeping, hole punch outcomes, and the
//! QUIC listener on the TCP listener's port.
use crate::{
    Error,
    peer_external::{Externals, Verdict},
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
    collections::{HashMap, HashSet},
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
    since: Instant,
    demoted: Option<Instant>,
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
            since: Instant::now(),
            demoted: None,
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

    /// What connection `id` of `peer`, admitted as `class`, retires. A direct
    /// data link retires the peer's circuits (§3.4). A circuit the peer dialed
    /// retires our direct links to it instead: a peer dials a circuit only
    /// while it holds no connection to us, so those links are stale (its
    /// process restarted) and would refuse the punch as a duplicate. A circuit
    /// we dialed beside a direct data link is itself redundant.
    pub(crate) fn superseded(
        &self,
        id: ConnectionId,
        peer: PeerId,
        class: Class,
    ) -> Vec<ConnectionId> {
        let inbound = self.0.get(&id).is_some_and(|remote| !remote.outbound);
        match class {
            Class::Data => self.relayed_of(peer),
            Class::Relayed if inbound => self.direct_of(peer),
            Class::Relayed if self.has_data(peer) => vec![id],
            Class::Relayed | Class::Auxiliary | Class::Holder | Class::Demoted => Vec::new(),
        }
    }

    fn direct_of(&self, peer: PeerId) -> Vec<ConnectionId> {
        self.0
            .iter()
            .filter(|(_, remote)| remote.peer == peer && remote.class != Class::Relayed)
            .map(|(id, _)| *id)
            .collect()
    }

    /// Holders admitted at least `grace` ago and links demoted at least
    /// `grace` ago, oldest first, with whether each is a demoted link we
    /// dialed to a public peer (an AutoNAT server or relay candidate).
    fn retirable(&self, grace: Duration) -> Vec<(ConnectionId, PeerId, bool)> {
        let mut retirable: Vec<(ConnectionId, PeerId, bool)> = self
            .0
            .iter()
            .filter(|(_, remote)| match remote.class {
                Class::Holder => remote.since.elapsed() >= grace,
                Class::Demoted => remote.demoted.is_some_and(|at| at.elapsed() >= grace),
                Class::Data | Class::Auxiliary | Class::Relayed => false,
            })
            .map(|(id, remote)| {
                let seeker = remote.class == Class::Demoted && Self::candidate(remote);
                (*id, remote.peer, seeker)
            })
            .collect();
        retirable.sort_by_key(|(id, ..)| *id);
        retirable
    }

    /// The peer's data connections, with whether we dialed each.
    pub(crate) fn data_of(&self, peer: PeerId) -> Vec<(ConnectionId, bool)> {
        self.0
            .iter()
            .filter(|(_, remote)| remote.peer == peer && remote.class == Class::Data)
            .map(|(id, remote)| (*id, remote.outbound))
            .collect()
    }

    pub(crate) fn reclass(&mut self, id: ConnectionId, class: Class) {
        if let Some(remote) = self.0.get_mut(&id) {
            remote.class = class;
            remote.demoted = (class == Class::Demoted).then(Instant::now);
        }
    }

    /// Peers whose data connection has been open `grace` without the peer
    /// proving it speaks this protocol on our chain.
    pub(crate) fn unproven(
        &self,
        grace: Duration,
        proven: impl Fn(PeerId) -> bool,
    ) -> HashSet<PeerId> {
        self.0
            .values()
            .filter(|remote| remote.class == Class::Data && remote.since.elapsed() >= grace)
            .map(|remote| remote.peer)
            .filter(|peer| !proven(*peer))
            .collect()
    }

    /// Connections demoted at least `grace` ago, with peer and direction.
    pub(crate) fn demoted_after(&self, grace: Duration) -> Vec<(ConnectionId, PeerId, bool)> {
        self.0
            .iter()
            .filter(|(_, remote)| remote.demoted.is_some_and(|at| at.elapsed() >= grace))
            .map(|(id, remote)| (*id, remote.peer, remote.outbound))
            .collect()
    }

    fn has_data(&self, peer: PeerId) -> bool {
        self.0
            .values()
            .any(|remote| remote.peer == peer && remote.class == Class::Data)
    }

    fn relayed_of(&self, peer: PeerId) -> Vec<ConnectionId> {
        self.0
            .iter()
            .filter(|(_, remote)| remote.peer == peer && remote.class == Class::Relayed)
            .map(|(id, _)| *id)
            .collect()
    }

    fn candidate(remote: &Remote) -> bool {
        remote.outbound
            && matches!(remote.class, Class::Data | Class::Demoted)
            && is_public(&remote.address)
    }

    /// Public peers we hold an outbound data (or demoted) connection to,
    /// oldest first, with the address we reached them on (without `/p2p`).
    fn relay_candidates(&self) -> Vec<(PeerId, Multiaddr)> {
        let mut candidates: Vec<(&ConnectionId, &Remote)> = self
            .0
            .iter()
            .filter(|(_, remote)| Self::candidate(remote))
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
    externals: Externals,
    probe_failed: bool,
    relay_open: Arc<AtomicBool>,
    local: PeerId,
    listen: Option<Multiaddr>,
    quic: bool,
    reservations: HashMap<ListenerId, PeerId>,
    backoff: HashMap<PeerId, Instant>,
    failures: HashMap<PeerId, u32>,
    circuits: HashMap<PeerId, u32>,
}

/// How long a relay rests after `failures` failed reservations in a row:
/// `RETRY` doubling up to `BACKOFF`. The first retry comes soon because the
/// usual refusal is our own stale reservation from before a restart, which
/// the relay drops once it notices the old connection is gone.
fn retry_delay(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(16);
    Reach::RETRY
        .saturating_mul(1 << doublings)
        .min(Reach::BACKOFF)
}

impl Reach {
    const RESERVATIONS: usize = 2;
    const RETRY: Duration = Duration::from_secs(30);
    const BACKOFF: Duration = Duration::from_secs(10 * 60);
    const HOLDER_GRACE: Duration = Duration::from_secs(10);

    pub(crate) fn new(
        forced: ReachMode,
        announce: Vec<Multiaddr>,
        relay_open: Arc<AtomicBool>,
        (local, listen): (PeerId, Option<Multiaddr>),
    ) -> Self {
        Self {
            forced,
            externals: Externals::new(local, announce),
            probe_failed: false,
            relay_open,
            local,
            listen,
            quic: false,
            reservations: HashMap::new(),
            backoff: HashMap::new(),
            failures: HashMap::new(),
            circuits: HashMap::new(),
        }
    }

    fn back_off(&mut self, relay: PeerId, now: Instant) {
        let failures = self.failures.entry(relay).or_default();
        *failures = failures.saturating_add(1);
        self.backoff.insert(relay, now + retry_delay(*failures));
    }

    /// Public with an announced or AutoNAT-confirmed direct address, private
    /// once a probe failed without one, unknown before that; forced in tests.
    pub(crate) fn update(&self, nat: &Nat) {
        let observed = match (
            self.externals.announced() || !nat.external().is_empty(),
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
            SwarmEvent::Behaviour(BehaviourEvent::Autonat(event)) => {
                self.autonat(swarm, &event, nat)
            }
            SwarmEvent::Behaviour(BehaviourEvent::AutonatServer(event)) => {
                tracing::debug!(?event, "AutoNAT probe served")
            }
            SwarmEvent::Behaviour(BehaviourEvent::Relay(event)) => self.relay_server(&event, nat),
            SwarmEvent::Behaviour(BehaviourEvent::RelayClient(event)) => {
                self.relay_client(&event, nat)
            }
            SwarmEvent::Behaviour(BehaviourEvent::Dcutr(event)) => {
                Self::dcutr(swarm, remotes, event)
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

    /// A failure counts toward private whoever reports it; a success is
    /// advertised only as `Externals` judges it.
    fn autonat(
        &mut self,
        swarm: &mut Swarm<Behaviour>,
        event: &autonat::v2::client::Event,
        nat: &Nat,
    ) {
        if let Err(error) = &event.result {
            tracing::debug!(%error, address = %event.tested_addr, "AutoNAT probe failed");
            self.probe_failed = true;
            self.update(nat);
            return;
        }
        match self.externals.verdict(&event.tested_addr, event.server) {
            Verdict::Confirm(address) => self.advertise(swarm, address, nat),
            Verdict::Retry(address) => swarm.behaviour_mut().candidates.offer(address),
            Verdict::Ignore => {
                tracing::debug!(address = %event.tested_addr, server = %event.server,
                    "AutoNAT confirmation not counted")
            }
        }
    }

    /// `witness`, a peer we dialed directly, sees us at `observed`: an AutoNAT
    /// candidate, never an advertised address on its own, and evidence that a
    /// confirmed address of the same transport no longer maps to us.
    pub(crate) fn observed(
        &mut self,
        (swarm, remotes): (&mut Swarm<Behaviour>, &Remotes),
        (observed, witness): (Multiaddr, PeerId),
        nat: &Nat,
    ) {
        if !direct_shape(&observed) {
            return;
        }
        let reported = (witness, Instant::now());
        let data = |peer: &PeerId| remotes.has_data(*peer);
        for stale in self
            .externals
            .witnessed(&observed, reported, &nat.external(), data)
        {
            tracing::info!(%stale, now = %observed, "external address mapping changed");
            swarm.remove_external_address(&stale);
            self.external(stale, false, nat);
        }
        if self.externals.flooded(witness) {
            tracing::debug!(%witness, "witness reports too many addresses; ignored");
            return;
        }
        swarm.behaviour_mut().candidates.offer(observed);
    }

    /// Advertises `address` as ours. The swarm reports no event for its own
    /// additions, so the NAT state is updated here.
    pub(crate) fn advertise(&self, swarm: &mut Swarm<Behaviour>, address: Multiaddr, nat: &Nat) {
        swarm.add_external_address(address.clone());
        self.external(address, true, nat);
    }

    /// Circuit addresses the relay client confirms stay out of the direct set,
    /// and with announced addresses nothing else is advertised.
    pub(crate) fn external(&self, address: Multiaddr, confirmed: bool, nat: &Nat) {
        if PeerLink::of(&address) == PeerLink::Direct && self.externals.admits(&address) {
            nat.set_external(address, confirmed);
            self.update(nat);
        }
    }

    fn relay_server(&mut self, event: &relay::Event, nat: &Nat) {
        match event {
            relay::Event::ReservationReqAccepted { src_peer_id, .. } => {
                nat.set_reserved_by(*src_peer_id, true)
            }
            relay::Event::ReservationClosed { src_peer_id }
            | relay::Event::ReservationTimedOut { src_peer_id } => {
                nat.set_reserved_by(*src_peer_id, false)
            }
            relay::Event::CircuitReqAccepted {
                src_peer_id,
                dst_peer_id,
            } => {
                for peer in [src_peer_id, dst_peer_id] {
                    *self.circuits.entry(*peer).or_default() += 1;
                }
            }
            relay::Event::CircuitClosed {
                src_peer_id,
                dst_peer_id,
                ..
            } => {
                for peer in [src_peer_id, dst_peer_id] {
                    self.circuit_closed(*peer);
                }
            }
            _ => (),
        }
    }

    fn circuit_closed(&mut self, peer: PeerId) {
        let Some(count) = self.circuits.get_mut(&peer) else {
            return;
        };
        *count -= 1;
        if *count == 0 {
            self.circuits.remove(&peer);
        }
    }

    /// A holder (admitted past the data budget, or demoted from data) must
    /// carry a reservation either way or a circuit within the grace period.
    /// While this node's reachability is unknown, or it is private with relays
    /// missing, the oldest few demoted links it dialed to public peers stay
    /// too: AutoNAT and reservations need them, and they are ours, not cheap
    /// inbound links.
    fn retire_holders(&self, swarm: &mut Swarm<Behaviour>, remotes: &Remotes, nat: &Nat) {
        let relays = nat.relays();
        let seeking = match nat.reachability() {
            Reachability::Public => 0,
            Reachability::Unknown => Self::RESERVATIONS,
            Reachability::Private => Self::RESERVATIONS.saturating_sub(relays.len()),
        };
        let mut holding = nat.reserved_by_others();
        holding.extend(relays);
        let mut kept = 0;
        for (id, peer, seeker) in remotes.retirable(Self::HOLDER_GRACE) {
            if holding.contains(&peer) || self.circuits.contains_key(&peer) {
                continue;
            }
            if seeker && kept < seeking {
                kept += 1;
                continue;
            }
            tracing::debug!(%peer, "holder carries no reservation or circuit; closing");
            swarm.close_connection(id);
        }
    }

    /// An accepted reservation clears the relay's failure count.
    pub(crate) fn relay_client(&mut self, event: &relay::client::Event, nat: &Nat) {
        if let relay::client::Event::ReservationReqAccepted { relay_peer_id, .. } = event {
            nat.set_relay(*relay_peer_id, true);
            self.failures.remove(relay_peer_id);
        }
    }

    /// A punched direct link retires the circuit where DCUtR reports it;
    /// `punches` counts outcomes on both ends.
    pub(crate) fn dcutr(swarm: &mut Swarm<Behaviour>, remotes: &Remotes, event: dcutr::Event) {
        match event.result {
            Ok(_direct) => {
                for id in remotes.relayed_of(event.remote_peer_id) {
                    swarm.close_connection(id);
                }
            }
            Err(error) => {
                tracing::debug!(peer = %event.remote_peer_id, %error, "hole punch failed")
            }
        }
    }

    /// Finished hole punches, whichever end dialed the circuit: a success
    /// retires the peer's circuits, a failure is recorded and the next
    /// relayed contact tries again.
    fn punches(swarm: &mut Swarm<Behaviour>, remotes: &Remotes, nat: &Nat) {
        for (peer, outcome) in swarm.behaviour_mut().dcutr.drain(Instant::now()) {
            match outcome {
                Ok(()) => {
                    nat.punch_succeeded();
                    for id in remotes.relayed_of(peer) {
                        swarm.close_connection(id);
                    }
                }
                Err(error) => nat.punch_failed(format!("{peer}: {error}")),
            }
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
        if let Some(candidate) = self.externals.bound(address) {
            swarm.behaviour_mut().candidates.offer(candidate);
        }
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
        if let Err(error) = reason {
            tracing::debug!(%relay, %error, "relay reservation ended; retrying after backoff");
            self.back_off(relay, Instant::now());
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
        Self::punches(swarm, remotes, nat);
        self.retire_holders(swarm, remotes, nat);
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
                    self.back_off(relay, now);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Remotes, retry_delay};
    use crate::peer_limits::{Class, Classes};
    use libp2p::{
        Multiaddr, PeerId,
        core::{ConnectedPoint, Endpoint, transport::PortUse},
        swarm::ConnectionId,
    };

    #[test]
    fn a_failed_reservation_retries_after_thirty_seconds_doubling_to_ten_minutes() {
        let delays = [1, 2, 3, 4, 5, 6, 40].map(|failures| retry_delay(failures).as_secs());
        assert_eq!(delays, [30, 60, 120, 240, 480, 600, 600]);
    }

    fn admit(
        (remotes, classes): (&mut Remotes, &Classes),
        (id, peer): (usize, PeerId),
        (class, outbound): (Class, bool),
    ) -> ConnectionId {
        let id = ConnectionId::new_unchecked(id);
        classes.set(id, class);
        let address: Multiaddr = "/ip4/11.0.0.1/udp/1/quic-v1".parse().unwrap();
        let endpoint = match outbound {
            true => ConnectedPoint::Dialer {
                address,
                role_override: Endpoint::Dialer,
                port_use: PortUse::Reuse,
            },
            false => ConnectedPoint::Listener {
                local_addr: address.clone(),
                send_back_addr: address,
            },
        };
        remotes.admit((id, peer), &endpoint, classes);
        id
    }

    #[test]
    fn a_circuit_the_peer_dialed_retires_our_stale_direct_links_not_itself() {
        let (mut remotes, classes) = (Remotes::default(), Classes::default());
        let (peer, bystander) = (PeerId::random(), PeerId::random());
        let mut set = |entry, kind| admit((&mut remotes, &classes), entry, kind);
        let stale = set((1, peer), (Class::Data, true));
        let holder = set((2, peer), (Class::Holder, false));
        set((3, bystander), (Class::Data, true));
        let circuit = set((4, peer), (Class::Relayed, false));
        let mut retired = remotes.superseded(circuit, peer, Class::Relayed);
        retired.sort();
        assert_eq!(
            retired,
            vec![stale, holder],
            "the peer dialed: ours are stale"
        );
        remotes.remove(stale);
        remotes.remove(holder);
        let punched = admit((&mut remotes, &classes), (5, peer), (Class::Data, true));
        assert_eq!(
            remotes.superseded(punched, peer, Class::Data),
            vec![circuit]
        );
        let ours = admit((&mut remotes, &classes), (6, peer), (Class::Relayed, true));
        assert_eq!(
            remotes.superseded(ours, peer, Class::Relayed),
            vec![ours],
            "a circuit we dialed beside a data link is redundant"
        );
    }
}
