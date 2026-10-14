//! Fixed connection admission limits for the public record transport.
//! Compose this behaviour before request-response so denial precedes its setup.
//! Only pending and established connection IDs are retained, never peer history.
//!
//! Every admitted connection gets a class. Data connections (one per peer,
//! 8 per direction, 16 in total) are the only ones that carry records. The
//! others have their own budgets and never carry data: one relay circuit per
//! peer, one auxiliary second direct connection per peer (AutoNAT dial-backs,
//! crossing hole punch dials), and reservation holders admitted inbound past
//! the data budget while this node's relay server is open.
use crate::{DialLimit, peer_gossip::guard, peer_nat::PeerLink};
use libp2p::{
    Multiaddr, PeerId,
    core::{ConnectedPoint, Endpoint, transport::PortUse},
    swarm::{
        ConnectionDenied, ConnectionId, FromSwarm, NetworkBehaviour, THandler, THandlerInEvent,
        THandlerOutEvent, ToSwarm, dummy,
    },
};
use std::{
    collections::{HashMap, HashSet},
    convert::Infallible,
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
};

const PENDING_PER_DIRECTION: usize = 4;
pub(crate) const ESTABLISHED_PER_DIRECTION: usize = 8;
const ESTABLISHED_TOTAL: usize = 16;
const RELAYED_MAX: usize = 8;
const AUXILIARY_MAX: usize = 8;
const HOLDERS_MAX: usize = 128;

/// What an admitted connection may carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Class {
    Data,
    Auxiliary,
    Holder,
    /// A data connection released while it carries a relay reservation: off
    /// the data budget like a holder, but its record handlers stay.
    Demoted,
    Relayed,
}

/// The class of every admitted connection, shared with the behaviours that
/// fence protocols by it and with the network loop.
#[derive(Clone, Default)]
pub(crate) struct Classes(Arc<Mutex<HashMap<ConnectionId, Class>>>);

impl Classes {
    pub(crate) fn of(&self, id: ConnectionId) -> Option<Class> {
        guard(&self.0).get(&id).copied()
    }

    fn set(&self, id: ConnectionId, class: Class) {
        guard(&self.0).insert(id, class);
    }

    fn remove(&self, id: ConnectionId) {
        guard(&self.0).remove(&id);
    }
}

/// Classes already held, counted for one admission.
#[derive(Default)]
struct Held {
    data: usize,
    data_direction: usize,
    relayed: usize,
    auxiliary: usize,
    holders: usize,
    peer_direct: bool,
    peer_auxiliary: bool,
    peer_relayed: bool,
}

/// Typed local admission refusal, recognisable by downcast instead of text.
#[derive(Debug)]
pub(crate) struct LimitDenied(pub(crate) DialLimit);

impl fmt::Display for LimitDenied {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "peer {} reached", self.0)
    }
}

impl std::error::Error for LimitDenied {}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    Inbound,
    Outbound,
}

#[derive(Default)]
pub(crate) struct Limits {
    pending_in: HashSet<ConnectionId>,
    pending_out: HashSet<ConnectionId>,
    established: HashMap<ConnectionId, (PeerId, Direction, Class)>,
    classes: Classes,
    relay_open: Arc<AtomicBool>,
}

fn denied(limit: DialLimit) -> ConnectionDenied {
    ConnectionDenied::new(LimitDenied(limit))
}

fn reserve(pending: &mut HashSet<ConnectionId>, id: ConnectionId) -> Result<(), ConnectionDenied> {
    if !pending.contains(&id) {
        if pending.len() >= PENDING_PER_DIRECTION {
            return Err(denied(DialLimit::Pending));
        }
        pending.insert(id);
    }
    Ok(())
}

/// How a connection reached admission: its direction, whether it rides a
/// relay circuit, and whether we dialed it from a fresh port (AutoNAT
/// dial-backs do; regular and hole punch dials reuse the listener's).
#[derive(Clone, Copy)]
struct Arrival {
    direction: Direction,
    link: PeerLink,
    fresh_port: bool,
}

impl Limits {
    /// `relay_open` admits reservation holders past the data budget; the
    /// network loop sets it while this node's relay server runs.
    pub(crate) fn new(classes: Classes, relay_open: Arc<AtomicBool>) -> Self {
        Self {
            classes,
            relay_open,
            ..Self::default()
        }
    }

    /// A released data connection that carries a relay reservation stays
    /// open, demoted: it frees its data slot.
    pub(crate) fn demote(&mut self, id: ConnectionId) -> bool {
        self.reclass(id, Class::Data, Class::Demoted)
    }

    /// A demoted connection takes a data slot again while one is free and its
    /// peer holds no other data connection.
    pub(crate) fn promote(&mut self, id: ConnectionId) -> bool {
        let Some(&(peer, direction, _)) = self.established.get(&id) else {
            return false;
        };
        let held = self.held(id, peer, direction);
        let peer_data = self.established.iter().any(|(known, (other, _, class))| {
            *known != id && *other == peer && *class == Class::Data
        });
        let room = held.data < ESTABLISHED_TOTAL && held.data_direction < ESTABLISHED_PER_DIRECTION;
        room && !peer_data && self.reclass(id, Class::Demoted, Class::Data)
    }

    fn reclass(&mut self, id: ConnectionId, from: Class, to: Class) -> bool {
        let Some(entry) = self.established.get_mut(&id) else {
            return false;
        };
        if entry.2 != from {
            return false;
        }
        entry.2 = to;
        self.classes.set(id, to);
        true
    }

    fn remove_pending(&mut self, id: ConnectionId) {
        self.pending_in.remove(&id);
        self.pending_out.remove(&id);
    }

    fn forget(&mut self, id: ConnectionId) {
        self.remove_pending(id);
        self.established.remove(&id);
        self.classes.remove(id);
    }

    fn held(&self, id: ConnectionId, peer: PeerId, direction: Direction) -> Held {
        let mut held = Held::default();
        let others = self.established.iter().filter(|(known, _)| **known != id);
        for (held_peer, held_direction, class) in others.map(|(_, value)| value) {
            let same = *held_peer == peer;
            match class {
                Class::Data => {
                    held.data += 1;
                    held.data_direction += usize::from(*held_direction == direction);
                    held.peer_direct |= same;
                }
                Class::Holder | Class::Demoted => {
                    held.holders += 1;
                    held.peer_direct |= same;
                }
                Class::Auxiliary => {
                    held.auxiliary += 1;
                    held.peer_auxiliary |= same;
                }
                Class::Relayed => {
                    held.relayed += 1;
                    held.peer_relayed |= same;
                }
            }
        }
        held
    }

    fn classify(
        &self,
        id: ConnectionId,
        peer: PeerId,
        arrival: Arrival,
    ) -> Result<Class, DialLimit> {
        let held = self.held(id, peer, arrival.direction);
        if arrival.link == PeerLink::Relayed {
            return match (held.peer_relayed, held.relayed >= RELAYED_MAX) {
                (true, _) => Err(DialLimit::Duplicate),
                (false, true) => Err(DialLimit::Established),
                (false, false) => Ok(Class::Relayed),
            };
        }
        if held.peer_direct {
            let auxiliary = arrival.direction == Direction::Inbound || arrival.fresh_port;
            return match (
                auxiliary && !held.peer_auxiliary,
                held.auxiliary >= AUXILIARY_MAX,
            ) {
                (false, _) => Err(DialLimit::Duplicate),
                (true, true) => Err(DialLimit::Established),
                (true, false) => Ok(Class::Auxiliary),
            };
        }
        if held.data < ESTABLISHED_TOTAL && held.data_direction < ESTABLISHED_PER_DIRECTION {
            return Ok(Class::Data);
        }
        let holder = arrival.direction == Direction::Inbound
            && self.relay_open.load(Ordering::Acquire)
            && held.holders < HOLDERS_MAX;
        match holder {
            true => Ok(Class::Holder),
            false => Err(DialLimit::Established),
        }
    }

    fn admit(
        &mut self,
        id: ConnectionId,
        peer: PeerId,
        arrival: Arrival,
    ) -> Result<dummy::ConnectionHandler, ConnectionDenied> {
        // Handshake completion ends the pending phase even when admission fails.
        self.remove_pending(id);
        let class = self.classify(id, peer, arrival).map_err(denied)?;
        // A later behaviour can still deny this connection. Count it only when
        // the Swarm confirms establishment, not while constructing handlers;
        // the class is published now so fenced behaviours pick their handler.
        self.classes.set(id, class);
        Ok(dummy::ConnectionHandler)
    }
}

impl NetworkBehaviour for Limits {
    type ConnectionHandler = dummy::ConnectionHandler;
    type ToSwarm = Infallible;

    fn handle_pending_inbound_connection(
        &mut self,
        id: ConnectionId,
        _: &Multiaddr,
        _: &Multiaddr,
    ) -> Result<(), ConnectionDenied> {
        reserve(&mut self.pending_in, id)
    }

    fn handle_pending_outbound_connection(
        &mut self,
        id: ConnectionId,
        _: Option<PeerId>,
        _: &[Multiaddr],
        _: Endpoint,
    ) -> Result<Vec<Multiaddr>, ConnectionDenied> {
        reserve(&mut self.pending_out, id)?;
        Ok(Vec::new())
    }

    fn handle_established_inbound_connection(
        &mut self,
        id: ConnectionId,
        peer: PeerId,
        local: &Multiaddr,
        remote: &Multiaddr,
    ) -> Result<THandler<Self>, ConnectionDenied> {
        let relayed = PeerLink::of(local) == PeerLink::Relayed;
        let arrival = Arrival {
            direction: Direction::Inbound,
            link: match relayed {
                true => PeerLink::Relayed,
                false => PeerLink::of(remote),
            },
            fresh_port: false,
        };
        self.admit(id, peer, arrival)
    }

    fn handle_established_outbound_connection(
        &mut self,
        id: ConnectionId,
        peer: PeerId,
        address: &Multiaddr,
        _: Endpoint,
        port_use: PortUse,
    ) -> Result<THandler<Self>, ConnectionDenied> {
        let arrival = Arrival {
            direction: Direction::Outbound,
            link: PeerLink::of(address),
            fresh_port: port_use == PortUse::New,
        };
        self.admit(id, peer, arrival)
    }

    fn on_swarm_event(&mut self, event: FromSwarm) {
        match event {
            FromSwarm::ConnectionEstablished(event) => {
                self.remove_pending(event.connection_id);
                let direction = match event.endpoint {
                    ConnectedPoint::Listener { .. } => Direction::Inbound,
                    ConnectedPoint::Dialer { .. } => Direction::Outbound,
                };
                let Some(class) = self.classes.of(event.connection_id) else {
                    return;
                };
                self.established.entry(event.connection_id).or_insert((
                    event.peer_id,
                    direction,
                    class,
                ));
            }
            FromSwarm::ConnectionClosed(event) => self.forget(event.connection_id),
            FromSwarm::DialFailure(event) => self.forget(event.connection_id),
            FromSwarm::ListenFailure(event) => self.forget(event.connection_id),
            _ => {}
        }
    }

    fn on_connection_handler_event(
        &mut self,
        _: PeerId,
        _: ConnectionId,
        event: THandlerOutEvent<Self>,
    ) {
        match event {}
    }

    fn poll(&mut self, _: &mut Context<'_>) -> Poll<ToSwarm<Self::ToSwarm, THandlerInEvent<Self>>> {
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libp2p::swarm::{
        DialError, ListenError,
        behaviour::{ConnectionClosed, ConnectionEstablished, DialFailure, ListenFailure},
    };

    fn id(value: usize) -> ConnectionId {
        ConnectionId::new_unchecked(value)
    }

    fn address() -> Multiaddr {
        "/ip4/127.0.0.1/tcp/1234".parse().unwrap()
    }

    fn endpoint(direction: Direction) -> ConnectedPoint {
        match direction {
            Direction::Inbound => ConnectedPoint::Listener {
                local_addr: address(),
                send_back_addr: address(),
            },
            Direction::Outbound => ConnectedPoint::Dialer {
                address: address(),
                role_override: Endpoint::Dialer,
                port_use: PortUse::New,
            },
        }
    }

    fn begin(
        limits: &mut Limits,
        id: ConnectionId,
        direction: Direction,
    ) -> Result<(), ConnectionDenied> {
        match direction {
            Direction::Inbound => {
                limits.handle_pending_inbound_connection(id, &address(), &address())
            }
            Direction::Outbound => limits
                .handle_pending_outbound_connection(id, None, &[], Endpoint::Dialer)
                .map(|_| ()),
        }
    }

    fn admit(
        limits: &mut Limits,
        id: ConnectionId,
        peer: PeerId,
        direction: Direction,
    ) -> Result<(), ConnectionDenied> {
        match direction {
            Direction::Inbound => {
                limits.handle_established_inbound_connection(id, peer, &address(), &address())
            }
            Direction::Outbound => limits.handle_established_outbound_connection(
                id,
                peer,
                &address(),
                Endpoint::Dialer,
                PortUse::Reuse,
            ),
        }
        .map(|_| ())
    }

    fn class(limits: &Limits, id: ConnectionId) -> Class {
        limits.classes.of(id).unwrap()
    }

    fn circuit() -> Multiaddr {
        let relay = PeerId::random();
        format!("/ip4/1.2.3.4/tcp/1/p2p/{relay}/p2p-circuit")
            .parse()
            .unwrap()
    }

    fn established(limits: &mut Limits, id: ConnectionId, peer: PeerId, direction: Direction) {
        limits.on_swarm_event(FromSwarm::ConnectionEstablished(ConnectionEstablished {
            peer_id: peer,
            connection_id: id,
            endpoint: &endpoint(direction),
            failed_addresses: &[],
            other_established: 0,
        }));
    }

    fn close(limits: &mut Limits, id: ConnectionId, peer: PeerId, direction: Direction) {
        limits.on_swarm_event(FromSwarm::ConnectionClosed(ConnectionClosed {
            peer_id: peer,
            connection_id: id,
            endpoint: &endpoint(direction),
            cause: None,
            remaining_established: 0,
        }));
    }

    fn failed(limits: &mut Limits, id: ConnectionId, direction: Direction, denied_elsewhere: bool) {
        match direction {
            Direction::Inbound => {
                let error = if denied_elsewhere {
                    ListenError::Denied {
                        cause: denied(DialLimit::Pending),
                    }
                } else {
                    ListenError::Aborted
                };
                limits.on_swarm_event(FromSwarm::ListenFailure(ListenFailure {
                    local_addr: &address(),
                    send_back_addr: &address(),
                    error: &error,
                    connection_id: id,
                    peer_id: None,
                }));
            }
            Direction::Outbound => {
                let error = if denied_elsewhere {
                    DialError::Denied {
                        cause: denied(DialLimit::Pending),
                    }
                } else {
                    DialError::Aborted
                };
                limits.on_swarm_event(FromSwarm::DialFailure(DialFailure {
                    peer_id: None,
                    error: &error,
                    connection_id: id,
                }));
            }
        }
    }

    fn empty(limits: &Limits) {
        assert!(limits.pending_in.is_empty());
        assert!(limits.pending_out.is_empty());
        assert!(limits.established.is_empty());
    }

    #[test]
    fn pending_limits_are_independent_and_duplicate_callbacks_do_not_fill_slots() {
        let mut limits = Limits::default();
        for value in 0..4 {
            begin(&mut limits, id(value), Direction::Inbound).unwrap();
            begin(&mut limits, id(value + 4), Direction::Outbound).unwrap();
        }
        begin(&mut limits, id(0), Direction::Inbound).unwrap();
        begin(&mut limits, id(4), Direction::Outbound).unwrap();
        assert!(begin(&mut limits, id(8), Direction::Inbound).is_err());
        assert!(begin(&mut limits, id(9), Direction::Outbound).is_err());
        assert_eq!(limits.pending_in.len(), 4);
        assert_eq!(limits.pending_out.len(), 4);
        for value in 0..8 {
            let direction = if value < 4 {
                Direction::Inbound
            } else {
                Direction::Outbound
            };
            failed(&mut limits, id(value), direction, false);
        }
        empty(&limits);
    }

    #[test]
    fn established_direction_limits_allow_eight_each_and_sixteen_total() {
        let mut limits = Limits::default();
        for (direction, offset) in [(Direction::Inbound, 0), (Direction::Outbound, 20)] {
            for value in 0..ESTABLISHED_PER_DIRECTION {
                let id = id(offset + value);
                let peer = PeerId::random();
                begin(&mut limits, id, direction).unwrap();
                admit(&mut limits, id, peer, direction).unwrap();
                established(&mut limits, id, peer, direction);
            }
            let denied_id = id(offset + ESTABLISHED_PER_DIRECTION);
            begin(&mut limits, denied_id, direction).unwrap();
            assert!(admit(&mut limits, denied_id, PeerId::random(), direction).is_err());
            assert!(!limits.pending_in.contains(&denied_id));
            assert!(!limits.pending_out.contains(&denied_id));
        }
        assert_eq!(limits.established.len(), ESTABLISHED_TOTAL);
        for direction in [Direction::Inbound, Direction::Outbound] {
            assert!(admit(&mut limits, id(99), PeerId::random(), direction).is_err());
        }
    }

    #[test]
    fn a_peer_has_one_data_connection_across_both_directions() {
        for direction in [Direction::Inbound, Direction::Outbound] {
            let mut limits = Limits::default();
            let peer = PeerId::random();
            let opposite = if direction == Direction::Inbound {
                Direction::Outbound
            } else {
                Direction::Inbound
            };
            begin(&mut limits, id(1), direction).unwrap();
            admit(&mut limits, id(1), peer, direction).unwrap();
            established(&mut limits, id(1), peer, direction);
            assert_eq!(class(&limits, id(1)), Class::Data);
            begin(&mut limits, id(2), Direction::Outbound).unwrap();
            assert!(admit(&mut limits, id(2), peer, Direction::Outbound).is_err());
            assert!(limits.pending_in.is_empty());
            assert!(limits.pending_out.is_empty());
            close(&mut limits, id(1), peer, direction);
            begin(&mut limits, id(3), opposite).unwrap();
            admit(&mut limits, id(3), peer, opposite).unwrap();
            assert_eq!(class(&limits, id(3)), Class::Data);
        }
    }

    #[test]
    fn a_direct_peer_gets_one_auxiliary_and_one_relayed_connection_besides_data() {
        let mut limits = Limits::default();
        let peer = PeerId::random();
        admit(&mut limits, id(1), peer, Direction::Outbound).unwrap();
        established(&mut limits, id(1), peer, Direction::Outbound);
        // An AutoNAT dial-back or a crossing punch dial: inbound, or a fresh port.
        admit(&mut limits, id(2), peer, Direction::Inbound).unwrap();
        established(&mut limits, id(2), peer, Direction::Inbound);
        assert_eq!(class(&limits, id(2)), Class::Auxiliary);
        assert!(admit(&mut limits, id(3), peer, Direction::Inbound).is_err());
        let fresh = limits.handle_established_outbound_connection(
            id(3),
            peer,
            &address(),
            Endpoint::Dialer,
            PortUse::New,
        );
        assert!(fresh.is_err(), "the auxiliary slot is taken");
        close(&mut limits, id(2), peer, Direction::Inbound);
        limits
            .handle_established_outbound_connection(
                id(3),
                peer,
                &address(),
                Endpoint::Dialer,
                PortUse::New,
            )
            .unwrap();
        assert_eq!(class(&limits, id(3)), Class::Auxiliary);
        let relayed = |limits: &mut Limits, id| {
            limits.handle_established_inbound_connection(id, peer, &circuit(), &address())
        };
        relayed(&mut limits, id(4)).unwrap();
        established(&mut limits, id(4), peer, Direction::Inbound);
        assert_eq!(class(&limits, id(4)), Class::Relayed);
        assert!(relayed(&mut limits, id(5)).is_err());
        assert_eq!(limits.established.len(), 2, "id(3) awaits establishment");
    }

    #[test]
    fn reservation_holders_pass_the_data_budget_only_while_the_relay_is_open() {
        let relay_open = Arc::new(AtomicBool::new(false));
        let mut limits = Limits::new(Classes::default(), Arc::clone(&relay_open));
        for value in 0..ESTABLISHED_PER_DIRECTION {
            let peer = PeerId::random();
            admit(&mut limits, id(value), peer, Direction::Inbound).unwrap();
            established(&mut limits, id(value), peer, Direction::Inbound);
        }
        assert!(admit(&mut limits, id(50), PeerId::random(), Direction::Inbound).is_err());
        relay_open.store(true, Ordering::Release);
        admit(&mut limits, id(50), PeerId::random(), Direction::Inbound).unwrap();
        assert_eq!(class(&limits, id(50)), Class::Holder);
        for value in 0..ESTABLISHED_PER_DIRECTION {
            let peer = PeerId::random();
            admit(&mut limits, id(20 + value), peer, Direction::Outbound).unwrap();
            established(&mut limits, id(20 + value), peer, Direction::Outbound);
        }
        let outbound = admit(&mut limits, id(51), PeerId::random(), Direction::Outbound);
        assert!(outbound.is_err(), "we never dial past the data budget");
    }

    #[test]
    fn a_demoted_connection_frees_its_data_slot_and_takes_one_back_only_when_free() {
        let mut limits = Limits::default();
        let kept = PeerId::random();
        admit(&mut limits, id(0), kept, Direction::Outbound).unwrap();
        established(&mut limits, id(0), kept, Direction::Outbound);
        for value in 1..ESTABLISHED_PER_DIRECTION {
            let peer = PeerId::random();
            admit(&mut limits, id(value), peer, Direction::Outbound).unwrap();
            established(&mut limits, id(value), peer, Direction::Outbound);
        }
        assert!(admit(&mut limits, id(20), PeerId::random(), Direction::Outbound).is_err());
        assert!(limits.demote(id(0)));
        assert!(!limits.demote(id(0)), "only a data connection is demoted");
        assert_eq!(class(&limits, id(0)), Class::Demoted);
        let newcomer = PeerId::random();
        admit(&mut limits, id(20), newcomer, Direction::Outbound).unwrap();
        established(&mut limits, id(20), newcomer, Direction::Outbound);
        assert_eq!(
            class(&limits, id(20)),
            Class::Data,
            "the freed slot is reused"
        );
        assert!(!limits.promote(id(0)), "no data slot is free");
        close(&mut limits, id(20), newcomer, Direction::Outbound);
        assert!(limits.promote(id(0)));
        assert_eq!(class(&limits, id(0)), Class::Data);
        assert!(
            !limits.promote(id(0)),
            "only a demoted connection is promoted"
        );
    }

    #[test]
    fn a_demoted_peer_is_still_direct_for_admission() {
        let mut limits = Limits::default();
        let peer = PeerId::random();
        admit(&mut limits, id(1), peer, Direction::Outbound).unwrap();
        established(&mut limits, id(1), peer, Direction::Outbound);
        assert!(limits.demote(id(1)));
        assert!(
            admit(&mut limits, id(2), peer, Direction::Outbound).is_err(),
            "a second direct dial is a duplicate, not a new data connection"
        );
        admit(&mut limits, id(3), peer, Direction::Inbound).unwrap();
        assert_eq!(class(&limits, id(3)), Class::Auxiliary);
    }

    #[test]
    fn failed_handshakes_release_pending_slots_in_both_directions() {
        for direction in [Direction::Inbound, Direction::Outbound] {
            let mut limits = Limits::default();
            for value in 0..4 {
                begin(&mut limits, id(value), direction).unwrap();
            }
            failed(&mut limits, id(0), direction, false);
            begin(&mut limits, id(4), direction).unwrap();
            for value in 1..5 {
                failed(&mut limits, id(value), direction, false);
            }
            empty(&limits);
        }
    }

    #[test]
    fn downstream_denial_releases_state_without_counting_establishment() {
        for direction in [Direction::Inbound, Direction::Outbound] {
            let mut limits = Limits::default();
            for after_admission in [false, true] {
                begin(&mut limits, id(1), direction).unwrap();
                if after_admission {
                    admit(&mut limits, id(1), PeerId::random(), direction).unwrap();
                }
                assert!(limits.established.is_empty());
                failed(&mut limits, id(1), direction, true);
                empty(&limits);
            }
        }
    }

    #[test]
    fn distinct_peer_churn_retains_no_peer_history() {
        let mut limits = Limits::default();
        for value in 0..1000 {
            let direction = if value % 2 == 0 {
                Direction::Inbound
            } else {
                Direction::Outbound
            };
            let peer = PeerId::random();
            begin(&mut limits, id(value), direction).unwrap();
            admit(&mut limits, id(value), peer, direction).unwrap();
            established(&mut limits, id(value), peer, direction);
            close(&mut limits, id(value), peer, direction);
            empty(&limits);
        }
    }

    #[test]
    fn duplicate_established_events_and_redundant_cleanup_are_harmless() {
        let mut limits = Limits::default();
        let peer = PeerId::random();
        begin(&mut limits, id(1), Direction::Inbound).unwrap();
        admit(&mut limits, id(1), peer, Direction::Inbound).unwrap();
        established(&mut limits, id(1), peer, Direction::Inbound);
        established(&mut limits, id(1), peer, Direction::Inbound);
        admit(&mut limits, id(1), peer, Direction::Inbound).unwrap();
        assert_eq!(limits.established.len(), 1);
        close(&mut limits, id(1), peer, Direction::Inbound);
        close(&mut limits, id(1), peer, Direction::Inbound);
        failed(&mut limits, id(1), Direction::Inbound, false);
        failed(&mut limits, id(1), Direction::Outbound, false);
        empty(&limits);
    }
}
