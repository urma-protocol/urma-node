//! Behaviour wrappers that hold libp2p to the overlay's policy. The records
//! protocol (data and Peers gossip alike) runs only on data connections: a
//! relay circuit, an auxiliary second connection or a reservation holder
//! never negotiates it, in either direction. Identify's raw observations
//! never become address candidates on their own; the network loop offers
//! only the ones it accepts, through `Candidates`.
use crate::peer_limits::{Class, Classes};
use either::Either;
use libp2p::{
    Multiaddr, PeerId,
    core::{Endpoint, transport::PortUse},
    multiaddr::Protocol,
    swarm::{
        ConnectionDenied, ConnectionId, FromSwarm, NetworkBehaviour, THandler, THandlerInEvent,
        THandlerOutEvent, ToSwarm,
        behaviour::{ConnectionClosed, ConnectionEstablished},
        dummy,
    },
};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    convert::Infallible,
    task::{Context, Poll},
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fence {
    DataOnly,
    NoCandidates,
}

pub(crate) struct Fenced<B> {
    inner: B,
    fence: Fence,
    classes: Classes,
    outside: HashSet<ConnectionId>,
    inside: HashMap<PeerId, HashSet<ConnectionId>>,
}

impl<B> Fenced<B> {
    /// Runs `inner` only on connections admitted as data connections.
    pub(crate) fn data_only(inner: B, classes: Classes) -> Self {
        Self {
            inner,
            fence: Fence::DataOnly,
            classes,
            outside: HashSet::new(),
            inside: HashMap::new(),
        }
    }

    /// Runs `inner` everywhere but drops the address candidates it reports.
    pub(crate) fn without_candidates(inner: B) -> Self {
        Self {
            inner,
            fence: Fence::NoCandidates,
            classes: Classes::default(),
            outside: HashSet::new(),
            inside: HashMap::new(),
        }
    }

    pub(crate) fn inner_mut(&mut self) -> &mut B {
        &mut self.inner
    }

    /// Drops a seen connection; returns how many of the peer's remain seen.
    fn forget(&mut self, peer: PeerId, id: ConnectionId) -> usize {
        let Some(seen) = self.inside.get_mut(&peer) else {
            return 0;
        };
        seen.remove(&id);
        let remaining = seen.len();
        if remaining == 0 {
            self.inside.remove(&peer);
        }
        remaining
    }

    fn shut_out(&mut self, id: ConnectionId) -> bool {
        let outside = self.fence == Fence::DataOnly && self.classes.of(id) != Some(Class::Data);
        if outside {
            self.outside.insert(id);
        }
        outside
    }
}

impl<B: NetworkBehaviour> NetworkBehaviour for Fenced<B> {
    type ConnectionHandler = Either<THandler<B>, dummy::ConnectionHandler>;
    type ToSwarm = B::ToSwarm;

    fn handle_pending_inbound_connection(
        &mut self,
        id: ConnectionId,
        local: &Multiaddr,
        remote: &Multiaddr,
    ) -> Result<(), ConnectionDenied> {
        self.inner
            .handle_pending_inbound_connection(id, local, remote)
    }

    fn handle_established_inbound_connection(
        &mut self,
        id: ConnectionId,
        peer: PeerId,
        local: &Multiaddr,
        remote: &Multiaddr,
    ) -> Result<THandler<Self>, ConnectionDenied> {
        if self.shut_out(id) {
            return Ok(Either::Right(dummy::ConnectionHandler));
        }
        self.inner
            .handle_established_inbound_connection(id, peer, local, remote)
            .map(Either::Left)
    }

    fn handle_pending_outbound_connection(
        &mut self,
        id: ConnectionId,
        peer: Option<PeerId>,
        addresses: &[Multiaddr],
        role: Endpoint,
    ) -> Result<Vec<Multiaddr>, ConnectionDenied> {
        self.inner
            .handle_pending_outbound_connection(id, peer, addresses, role)
    }

    fn handle_established_outbound_connection(
        &mut self,
        id: ConnectionId,
        peer: PeerId,
        address: &Multiaddr,
        role: Endpoint,
        port_use: PortUse,
    ) -> Result<THandler<Self>, ConnectionDenied> {
        if self.shut_out(id) {
            return Ok(Either::Right(dummy::ConnectionHandler));
        }
        self.inner
            .handle_established_outbound_connection(id, peer, address, role, port_use)
            .map(Either::Left)
    }

    /// A shut-out connection never exists for `inner`, so the per-peer
    /// counts it receives cover only the connections it sees; failures still
    /// reach it, since its own dials may end in one.
    fn on_swarm_event(&mut self, event: FromSwarm) {
        match event {
            FromSwarm::ConnectionEstablished(event) => {
                if self.outside.contains(&event.connection_id) {
                    return;
                }
                let seen = self.inside.entry(event.peer_id).or_default();
                let other_established = seen.len();
                seen.insert(event.connection_id);
                self.inner.on_swarm_event(FromSwarm::ConnectionEstablished(
                    ConnectionEstablished {
                        other_established,
                        ..event
                    },
                ));
            }
            FromSwarm::ConnectionClosed(event) => {
                if self.outside.remove(&event.connection_id) {
                    return;
                }
                let remaining_established = self.forget(event.peer_id, event.connection_id);
                self.inner
                    .on_swarm_event(FromSwarm::ConnectionClosed(ConnectionClosed {
                        remaining_established,
                        ..event
                    }));
            }
            FromSwarm::AddressChange(ref change)
                if self.outside.contains(&change.connection_id) => {}
            FromSwarm::DialFailure(ref failure) => {
                self.outside.remove(&failure.connection_id);
                self.inner.on_swarm_event(event);
            }
            FromSwarm::ListenFailure(ref failure) => {
                self.outside.remove(&failure.connection_id);
                self.inner.on_swarm_event(event);
            }
            other => self.inner.on_swarm_event(other),
        }
    }

    fn on_connection_handler_event(
        &mut self,
        peer: PeerId,
        id: ConnectionId,
        event: THandlerOutEvent<Self>,
    ) {
        match event {
            Either::Left(event) => self.inner.on_connection_handler_event(peer, id, event),
            Either::Right(never) => match never {},
        }
    }

    fn poll(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<ToSwarm<Self::ToSwarm, THandlerInEvent<Self>>> {
        loop {
            match self.inner.poll(cx) {
                Poll::Ready(ToSwarm::NewExternalAddrCandidate(_))
                    if self.fence == Fence::NoCandidates => {}
                Poll::Ready(action) => return Poll::Ready(action.map_in(Either::Left)),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// Our own public addresses as peers saw them over direct connections we
/// dialed, offered to AutoNAT (to confirm) and DCUtR (to punch). Every offer
/// re-announces the known set with TCP before QUIC: DCUtR keeps the most
/// recent first, so hole punching tries QUIC first.
#[derive(Default)]
pub(crate) struct Candidates {
    known: Vec<Multiaddr>,
    queue: VecDeque<Multiaddr>,
}

impl Candidates {
    const KNOWN_MAX: usize = 8;

    pub(crate) fn offer(&mut self, address: Multiaddr) {
        if self.known.contains(&address) {
            return;
        }
        if self.known.len() == Self::KNOWN_MAX {
            self.known.remove(0);
        }
        self.known.push(address);
        self.known.sort_by_key(is_quic);
        self.queue.extend(self.known.iter().cloned());
    }
}

fn is_quic(address: &Multiaddr) -> bool {
    address.iter().any(|part| part == Protocol::QuicV1)
}

impl NetworkBehaviour for Candidates {
    type ConnectionHandler = dummy::ConnectionHandler;
    type ToSwarm = Infallible;

    fn handle_established_inbound_connection(
        &mut self,
        _: ConnectionId,
        _: PeerId,
        _: &Multiaddr,
        _: &Multiaddr,
    ) -> Result<THandler<Self>, ConnectionDenied> {
        Ok(dummy::ConnectionHandler)
    }

    fn handle_established_outbound_connection(
        &mut self,
        _: ConnectionId,
        _: PeerId,
        _: &Multiaddr,
        _: Endpoint,
        _: PortUse,
    ) -> Result<THandler<Self>, ConnectionDenied> {
        Ok(dummy::ConnectionHandler)
    }

    fn on_swarm_event(&mut self, _: FromSwarm) {}

    fn on_connection_handler_event(
        &mut self,
        _: PeerId,
        _: ConnectionId,
        event: THandlerOutEvent<Self>,
    ) {
        match event {}
    }

    fn poll(&mut self, _: &mut Context<'_>) -> Poll<ToSwarm<Self::ToSwarm, THandlerInEvent<Self>>> {
        match self.queue.pop_front() {
            Some(address) => Poll::Ready(ToSwarm::NewExternalAddrCandidate(address)),
            None => Poll::Pending,
        }
    }
}
