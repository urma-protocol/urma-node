//! The overlay peers that inventory sync, parent recovery and record retrieval
//! use right now: configured peers always, then every connected peer that
//! spoke this protocol version on our chain, inbound or outbound. A connected
//! peer leaves the set with its last connection.
use crate::{
    Error,
    model::{
        OverlayStatus, PeerLink as LinkStatus, Reachability as ReachabilityStatus, RecordPackage,
    },
    peer::{Answer, CALL_TIMEOUT, Command, request},
    peer_gossip::Shared,
    peer_nat::{PeerLink, Reachability},
    peer_protocol::Kind,
    peerbook::PeerAddress,
    recorded::RecordedError,
    retrieval::RecordSource,
};
use bitcoin::Txid;
use libp2p::{Multiaddr, PeerId};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    time::Instant,
};
use tokio::sync::mpsc as async_mpsc;

/// The network thread's command queue and shared state, without ownership:
/// handles built from it never keep the network alive.
#[derive(Clone)]
pub(crate) struct Link {
    pub(crate) commands: async_mpsc::Sender<Command>,
    pub(crate) stop: Arc<AtomicBool>,
    pub(crate) shared: Arc<Shared>,
}

impl Link {
    /// Without addresses the request rides an existing connection.
    pub(crate) fn source(&self, peer: PeerId, addresses: Vec<Multiaddr>) -> PeerSource {
        PeerSource {
            peer,
            addresses,
            link: self.clone(),
        }
    }
}

/// A configured or connected untrusted package source. Metadata is matched to
/// the request; proof verification and independent chain selection belong to
/// the caller. Cloning a source never dials; fetch performs the bounded request.
#[derive(Clone)]
pub struct PeerSource {
    peer: PeerId,
    addresses: Vec<Multiaddr>,
    link: Link,
}

impl PeerSource {
    pub(crate) fn request<T>(
        &self,
        kind: Kind,
        limit: Instant,
        answer: fn(mpsc::SyncSender<Result<T, Error>>) -> Answer,
    ) -> Result<T, Error> {
        let target = (self.peer, self.addresses.clone());
        request(
            &self.link.commands,
            &self.link.stop,
            target,
            kind,
            limit,
            answer,
        )
    }

    /// Whether this peer has an established connection right now.
    pub(crate) fn connected(&self) -> bool {
        self.link.shared.is_connected(self.peer)
    }

    /// Whether it holds a data connection (a slot in our data budget).
    pub(crate) fn has_data(&self) -> bool {
        self.link.shared.has_direct(self.peer)
    }

    /// Whether one of its connections is one we dialed.
    pub(crate) fn outbound(&self) -> bool {
        self.link.shared.is_outbound(self.peer)
    }

    /// Configured with an address to dial, not only reached over a connection.
    pub(crate) fn configured(&self) -> bool {
        !self.addresses.is_empty()
    }

    /// Fire and forget; a full queue only delays the idle-connection timeout.
    pub(crate) fn disconnect(&self) {
        let _ = self.link.commands.try_send(Command::Disconnect(self.peer));
    }

    /// Sync no longer needs this peer: its data slot is freed, but a link
    /// carrying a relay reservation stays (`peer_release`).
    pub(crate) fn release(&self) {
        let _ = self.link.commands.try_send(Command::Release(self.peer));
    }
}

impl RecordSource for PeerSource {
    fn fetch(&self, txid: Txid) -> Result<Option<RecordPackage>, Error> {
        self.fetch_until(txid, Instant::now() + CALL_TIMEOUT)
    }
}

/// Configured peers, which are dialed when not connected, then the live ones:
/// those we dialed before those that dialed us. Cheap to clone; every call
/// reads the current overlay.
#[derive(Clone)]
pub struct SyncPeers {
    link: Link,
    manual: Vec<PeerAddress>,
    next: Arc<AtomicUsize>,
    next_inbound: Arc<AtomicUsize>,
}

impl SyncPeers {
    /// Record sources per retrieval call: retrieval tries this many.
    pub const RECORD_PEERS: usize = 4;

    pub(crate) fn new(link: Link, manual: Vec<PeerAddress>) -> Self {
        Self {
            link,
            manual,
            next: Arc::new(AtomicUsize::new(0)),
            next_inbound: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// One source per identity, labelled by it, skipping `omitting` peers.
    pub fn sources(&self, omitting: &[String]) -> Vec<(String, PeerSource)> {
        let configured = self
            .manual
            .iter()
            .map(|address| (address.peer_id(), vec![address.address().clone()]));
        let live = self
            .link
            .shared
            .live()
            .into_iter()
            .map(|peer| (peer, Vec::new()));
        let mut sources: Vec<(String, PeerSource)> = Vec::new();
        for (peer, addresses) in configured.chain(live) {
            let label = peer.to_string();
            if omitting.contains(&label) || sources.iter().any(|(known, _)| *known == label) {
                continue;
            }
            sources.push((label, self.link.source(peer, addresses)));
        }
        sources
    }

    /// Up to `size` current peers: every configured one first, then the live
    /// peers we dialed, then those that dialed us, each live group rotating
    /// across calls on its own. A crowd of live peers cannot push a configured
    /// peer out of the window, and inbound peers never come before outbound.
    pub(crate) fn window(&self, size: usize) -> Vec<PeerSource> {
        let peers = self.sources(&[]);
        let (configured, live): (Vec<PeerSource>, Vec<PeerSource>) = peers
            .into_iter()
            .map(|(_, source)| source)
            .partition(PeerSource::configured);
        let (outbound, inbound): (Vec<PeerSource>, Vec<PeerSource>) =
            live.into_iter().partition(PeerSource::outbound);
        let mut window: Vec<PeerSource> = configured.into_iter().take(size).collect();
        for (group, next) in [(&outbound, &self.next), (&inbound, &self.next_inbound)] {
            let room = size.saturating_sub(window.len()).min(group.len());
            if room > 0 {
                let start = next.fetch_add(room, Ordering::Relaxed) % group.len();
                window
                    .extend((0..room).map(|offset| group[(start + offset) % group.len()].clone()));
            }
        }
        window
    }

    /// Separate sources for one retrieval call, each verified on its own.
    pub fn record_sources(&self) -> Vec<PeerSource> {
        self.window(Self::RECORD_PEERS)
    }

    /// How many record sources a retrieval call would get right now.
    pub fn record_count(&self) -> usize {
        self.sources(&[]).len().min(Self::RECORD_PEERS)
    }

    /// The peer's link right now; `None` while it has no connection.
    pub(crate) fn link_of(&self, peer: PeerId) -> Option<LinkStatus> {
        let links = self.link.shared.links();
        let (_, link) = links.iter().find(|(known, _)| *known == peer)?;
        Some(match link {
            PeerLink::Direct => LinkStatus::Direct,
            PeerLink::Relayed => LinkStatus::Relayed,
        })
    }

    /// Reachability, this node's relays and hole punch counters.
    pub(crate) fn nat_status(&self, status: &mut OverlayStatus) {
        let nat = &self.link.shared.nat;
        status.reachability = match nat.reachability() {
            Reachability::Unknown => ReachabilityStatus::Unknown,
            Reachability::Public => ReachabilityStatus::Public,
            Reachability::Private => ReachabilityStatus::Private,
        };
        status.relays = nat.relays().iter().map(PeerId::to_string).collect();
        let punch = nat.hole_punch();
        let (last_error, last_error_at) = RecordedError::split(punch.last_error);
        status.hole_punch.attempts = punch.attempts;
        status.hole_punch.successes = punch.successes;
        status.hole_punch.last_error = last_error;
        status.hole_punch.last_error_at = last_error_at;
    }
}
