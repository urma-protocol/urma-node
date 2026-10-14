//! libp2p's relay server handler keeps its connection alive for a grace
//! period after the last reservation ends, but nothing wakes the connection
//! when that period passes: the swarm never re-reads keep-alive and an idle
//! data connection stays open forever. `Rewaken` re-polls the connection while
//! the inner handler holds it, so the grace period ends and the idle timeout
//! applies again. Live reservations keep the connection, as before.
use libp2p::{
    Multiaddr, PeerId,
    core::{Endpoint, transport::PortUse},
    swarm::{
        ConnectionDenied, ConnectionHandler, ConnectionHandlerEvent, ConnectionId, FromSwarm,
        NetworkBehaviour, SubstreamProtocol, THandler, THandlerInEvent, THandlerOutEvent, ToSwarm,
        handler::ConnectionEvent,
    },
};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::time::Sleep;

pub(crate) struct Rewaken<B>(pub(crate) B);

pub(crate) struct Waking<H> {
    inner: H,
    recheck: Option<Pin<Box<Sleep>>>,
}

impl<H> Waking<H> {
    const RECHECK: Duration = Duration::from_secs(5);

    fn new(inner: H) -> Self {
        Self {
            inner,
            recheck: None,
        }
    }
}

impl<B: NetworkBehaviour> NetworkBehaviour for Rewaken<B> {
    type ConnectionHandler = Waking<THandler<B>>;
    type ToSwarm = B::ToSwarm;

    fn handle_pending_inbound_connection(
        &mut self,
        id: ConnectionId,
        local: &Multiaddr,
        remote: &Multiaddr,
    ) -> Result<(), ConnectionDenied> {
        self.0.handle_pending_inbound_connection(id, local, remote)
    }

    fn handle_established_inbound_connection(
        &mut self,
        id: ConnectionId,
        peer: PeerId,
        local: &Multiaddr,
        remote: &Multiaddr,
    ) -> Result<THandler<Self>, ConnectionDenied> {
        self.0
            .handle_established_inbound_connection(id, peer, local, remote)
            .map(Waking::new)
    }

    fn handle_pending_outbound_connection(
        &mut self,
        id: ConnectionId,
        peer: Option<PeerId>,
        addresses: &[Multiaddr],
        role: Endpoint,
    ) -> Result<Vec<Multiaddr>, ConnectionDenied> {
        self.0
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
        self.0
            .handle_established_outbound_connection(id, peer, address, role, port_use)
            .map(Waking::new)
    }

    fn on_swarm_event(&mut self, event: FromSwarm) {
        self.0.on_swarm_event(event);
    }

    fn on_connection_handler_event(
        &mut self,
        peer: PeerId,
        id: ConnectionId,
        event: THandlerOutEvent<Self>,
    ) {
        self.0.on_connection_handler_event(peer, id, event);
    }

    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<ToSwarm<Self::ToSwarm, THandlerInEvent<Self>>> {
        self.0.poll(cx)
    }
}

impl<H: ConnectionHandler> ConnectionHandler for Waking<H> {
    type FromBehaviour = H::FromBehaviour;
    type ToBehaviour = H::ToBehaviour;
    type InboundProtocol = H::InboundProtocol;
    type OutboundProtocol = H::OutboundProtocol;
    type InboundOpenInfo = H::InboundOpenInfo;
    type OutboundOpenInfo = H::OutboundOpenInfo;

    fn listen_protocol(&self) -> SubstreamProtocol<Self::InboundProtocol, Self::InboundOpenInfo> {
        self.inner.listen_protocol()
    }

    fn connection_keep_alive(&self) -> bool {
        self.inner.connection_keep_alive()
    }

    fn poll(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<ConnectionHandlerEvent<Self::OutboundProtocol, Self::OutboundOpenInfo, Self::ToBehaviour>>
    {
        if let Poll::Ready(event) = self.inner.poll(cx) {
            return Poll::Ready(event);
        }
        if !self.inner.connection_keep_alive() {
            self.recheck = None;
            return Poll::Pending;
        }
        let recheck = self
            .recheck
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(Self::RECHECK)));
        if recheck.as_mut().poll(cx).is_ready() {
            self.recheck = None;
            cx.waker().wake_by_ref();
        }
        Poll::Pending
    }

    fn poll_close(&mut self, cx: &mut Context<'_>) -> Poll<Option<Self::ToBehaviour>> {
        self.inner.poll_close(cx)
    }

    fn on_behaviour_event(&mut self, event: Self::FromBehaviour) {
        self.inner.on_behaviour_event(event);
    }

    fn on_connection_event(
        &mut self,
        event: ConnectionEvent<
            Self::InboundProtocol,
            Self::OutboundProtocol,
            Self::InboundOpenInfo,
            Self::OutboundOpenInfo,
        >,
    ) {
        self.inner.on_connection_event(event);
    }
}
