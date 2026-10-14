//! Hole punch outcomes on both ends. libp2p's DCUtR reports a success only
//! where its own dial produced the direct connection, which is the end that
//! answered the circuit; the end that dialed the circuit dials too, but its
//! direct link usually arrives as the other end's connection (QUIC), and its
//! failures are never reported. `Punches` counts a punch from the first DCUtR
//! dial to a peer until any direct connection to that peer opens (success),
//! DCUtR reports an error, or `WAIT` passes (failure).
use libp2p::{
    Multiaddr, PeerId,
    core::{Endpoint, transport::PortUse},
    dcutr,
    swarm::{
        ConnectionDenied, ConnectionId, FromSwarm, NetworkBehaviour, THandler, THandlerInEvent,
        THandlerOutEvent, ToSwarm,
    },
};
use std::{
    collections::HashMap,
    task::{Context, Poll},
    time::{Duration, Instant},
};

/// A finished punch: the peer, and the reason when it failed.
pub(crate) type Outcome = (PeerId, Result<(), String>);

pub(crate) struct Punches {
    inner: dcutr::Behaviour,
    pending: HashMap<PeerId, Instant>,
    outcomes: Vec<Outcome>,
}

impl Punches {
    /// DCUtR's three attempts each wait out a dial; a punch not done by then
    /// has failed.
    const WAIT: Duration = Duration::from_secs(30);

    pub(crate) fn new(local: PeerId) -> Self {
        Self {
            inner: dcutr::Behaviour::new(local),
            pending: HashMap::new(),
            outcomes: Vec::new(),
        }
    }

    fn dialed(&mut self, peer: PeerId, at: Instant) {
        self.pending.entry(peer).or_insert(at);
    }

    fn resolve(&mut self, peer: PeerId, outcome: Result<(), String>) {
        if self.pending.remove(&peer).is_some() {
            self.outcomes.push((peer, outcome));
        }
    }

    /// Finished punches since the last call, expiring those older than `WAIT`.
    pub(crate) fn drain(&mut self, now: Instant) -> Vec<Outcome> {
        let expired: Vec<PeerId> = self
            .pending
            .iter()
            .filter(|(_, since)| now.duration_since(**since) >= Self::WAIT)
            .map(|(peer, _)| *peer)
            .collect();
        for peer in expired {
            self.resolve(peer, Err("no direct connection".into()));
        }
        std::mem::take(&mut self.outcomes)
    }
}

impl NetworkBehaviour for Punches {
    type ConnectionHandler = THandler<dcutr::Behaviour>;
    type ToSwarm = dcutr::Event;

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
        self.inner
            .handle_established_inbound_connection(id, peer, local, remote)
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
        self.inner
            .handle_established_outbound_connection(id, peer, address, role, port_use)
    }

    fn on_swarm_event(&mut self, event: FromSwarm) {
        if let FromSwarm::ConnectionEstablished(established) = &event
            && !established.endpoint.is_relayed()
        {
            self.resolve(established.peer_id, Ok(()));
        }
        self.inner.on_swarm_event(event);
    }

    fn on_connection_handler_event(
        &mut self,
        peer: PeerId,
        id: ConnectionId,
        event: THandlerOutEvent<Self>,
    ) {
        self.inner.on_connection_handler_event(peer, id, event);
    }

    fn poll(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<ToSwarm<Self::ToSwarm, THandlerInEvent<Self>>> {
        let polled = self.inner.poll(cx);
        match &polled {
            Poll::Ready(ToSwarm::Dial { opts }) => {
                if let Some(peer) = opts.get_peer_id() {
                    self.dialed(peer, Instant::now());
                }
            }
            Poll::Ready(ToSwarm::GenerateEvent(dcutr::Event {
                remote_peer_id,
                result: Err(error),
            })) => self.resolve(*remote_peer_id, Err(error.to_string())),
            Poll::Ready(_) | Poll::Pending => (),
        }
        polled
    }
}

#[cfg(test)]
mod tests {
    use super::Punches;
    use libp2p::PeerId;
    use std::time::{Duration, Instant};

    #[test]
    fn each_punch_counts_once_from_its_first_dial_to_a_direct_link_or_expiry() {
        let mut punches = Punches::new(PeerId::random());
        let (won, lost, unknown) = (PeerId::random(), PeerId::random(), PeerId::random());
        let start = Instant::now();
        punches.dialed(won, start);
        punches.dialed(won, start + Duration::from_secs(5));
        punches.dialed(lost, start);
        punches.resolve(unknown, Ok(()));
        punches.resolve(won, Ok(()));
        punches.resolve(won, Ok(()));
        assert_eq!(punches.drain(start), vec![(won, Ok(()))]);
        assert!(punches.drain(start + Duration::from_secs(29)).is_empty());
        let expired = punches.drain(start + Punches::WAIT);
        assert_eq!(expired, vec![(lost, Err("no direct connection".into()))]);
        punches.dialed(lost, start);
        punches.resolve(lost, Err("attempts exceeded".into()));
        punches.resolve(lost, Ok(()));
        assert_eq!(
            punches.drain(start),
            vec![(lost, Err("attempts exceeded".into()))],
            "a failed punch is not turned into a success by a later direct link"
        );
    }
}
