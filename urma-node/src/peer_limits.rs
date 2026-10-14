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
//!
//! A peer dialing us again from the very address of a link it already holds
//! has lost that link (its process restarted without closing it): the link
//! leaves every budget at once, the new connection takes its place, and it is
//! closed. Only that peer, authenticated by Noise, can do this, and only to
//! its own links; no budget grows by it.
use crate::{DialLimit, peer_gossip::guard, peer_nat::PeerLink};
use libp2p::{
    Multiaddr, PeerId,
    core::{ConnectedPoint, Endpoint, transport::PortUse},
    swarm::{
        CloseConnection, ConnectionDenied, ConnectionId, FromSwarm, NetworkBehaviour, THandler,
        THandlerInEvent, THandlerOutEvent, ToSwarm, dummy,
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

    pub(crate) fn set(&self, id: ConnectionId, class: Class) {
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
    /// Where each direct connection that dialed us came from.
    inbound: HashMap<ConnectionId, Multiaddr>,
    /// Stale links replaced by their peer's new connection: counted nowhere,
    /// closing.
    retiring: HashSet<ConnectionId>,
    closing: Vec<(PeerId, ConnectionId)>,
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

    /// A released data connection frees its data slot and counts as a holder
    /// instead; refused (false) once the holder budget is spent.
    pub(crate) fn demote(&mut self, id: ConnectionId) -> bool {
        let Some(&(peer, direction, _)) = self.established.get(&id) else {
            return false;
        };
        self.held(id, peer, direction).holders < HOLDERS_MAX
            && self.reclass(id, Class::Data, Class::Demoted)
    }

    /// A demoted connection we dialed takes a data slot again while one is
    /// free and its peer holds no other data connection. One that dialed us
    /// never does: inbound slots go to newcomers.
    pub(crate) fn promote(&mut self, id: ConnectionId) -> bool {
        let Some(&(peer, direction, _)) = self.established.get(&id) else {
            return false;
        };
        if direction != Direction::Outbound {
            return false;
        }
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
        self.inbound.remove(&id);
        self.retiring.remove(&id);
        self.classes.remove(id);
    }

    fn held(&self, id: ConnectionId, peer: PeerId, direction: Direction) -> Held {
        let mut held = Held::default();
        let others = self
            .established
            .iter()
            .filter(|(known, _)| **known != id && !self.retiring.contains(known));
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

    /// The peer's links that dialed us from `remote`, the very address this
    /// new connection `id` comes from: a live peer never dials us twice from
    /// one socket, while an AutoNAT dial-back comes from a fresh port.
    fn stale(&self, id: ConnectionId, peer: PeerId, remote: &Multiaddr) -> Vec<ConnectionId> {
        self.established
            .iter()
            .filter(|(known, (other, _, _))| {
                **known != id && *other == peer && self.inbound.get(known) == Some(remote)
            })
            .map(|(known, _)| *known)
            .collect()
    }

    fn retire(&mut self, peer: PeerId, stale: Vec<ConnectionId>) {
        for id in stale {
            if self.retiring.insert(id) {
                tracing::debug!(%peer, "peer redialed from the same address; stale link replaced");
                self.closing.push((peer, id));
            }
        }
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
        let link = match relayed {
            true => PeerLink::Relayed,
            false => PeerLink::of(remote),
        };
        if link == PeerLink::Direct {
            let stale = self.stale(id, peer, remote);
            self.retire(peer, stale);
            self.inbound.insert(id, remote.clone());
        }
        let arrival = Arrival {
            direction: Direction::Inbound,
            link,
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
        match self.closing.pop() {
            Some((peer_id, id)) => Poll::Ready(ToSwarm::CloseConnection {
                peer_id,
                connection: CloseConnection::One(id),
            }),
            None => Poll::Pending,
        }
    }
}

#[cfg(test)]
#[path = "peer_limits_tests.rs"]
mod tests;
