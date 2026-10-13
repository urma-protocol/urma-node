//! Bitcoin-style peer discovery. Seeds bootstrap a sparse book; afterwards
//! connected peers exchange verified addresses. A dial proves only that a
//! Noise identity on this chain answered at an address: never record
//! availability or honesty. Each Service round performs at most three
//! network operations and checks cancellation between them.
use crate::{
    Error,
    engine::Engine,
    model::{BookEntry, DiscoveryStatus},
    peer::PeerNetwork,
    peer_protocol::GossipPeer,
    peerbook::PeerBook,
    recorded::{RecordedError, read, unix_seconds as now},
};
use libp2p::{Multiaddr, PeerId, multiaddr::Protocol};
use rand::seq::SliceRandom;
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

pub struct Discovery {
    network: Arc<PeerNetwork>,
    book: Arc<Mutex<PeerBook>>,
    path: PathBuf,
    round: AtomicU64,
    seed_error: Mutex<Option<RecordedError>>,
    gossip_error: Mutex<Option<RecordedError>>,
    sync_backoff: Mutex<Vec<String>>,
    own_addresses: Mutex<Vec<Multiaddr>>,
}

/// Network operations left in this round; stop ends the round early.
struct Budget<'a> {
    left: usize,
    stop: &'a AtomicBool,
}

impl Budget<'_> {
    fn take(&mut self) -> bool {
        if self.left == 0 || self.stop.load(Ordering::Acquire) {
            return false;
        }
        self.left -= 1;
        true
    }
}

/// An address that can only ever be this node: it names our identity, or is
/// an IP literal (no name that might also resolve elsewhere).
fn names_self(address: &Multiaddr, local: &Protocol) -> bool {
    let literal = matches!(
        address.iter().next(),
        Some(Protocol::Ip4(_) | Protocol::Ip6(_))
    );
    literal || address.iter().any(|part| part == *local)
}

fn poisoned() -> Error {
    Error::Invalid("peer discovery state poisoned".into())
}

impl Discovery {
    /// Connected peers the round tries to reach.
    pub const TARGET: usize = 8;
    pub const DIALS_PER_ROUND: usize = 2;
    pub const OPERATIONS_PER_ROUND: usize = 3;
    /// Rounds between address requests to one random connected peer.
    pub const GOSSIP_EVERY: u64 = 30;
    pub const GOSSIP_LIMIT: u32 = 32;
    /// Rounds between seed passes while below target but connected.
    pub const SEED_EVERY: u64 = 600;
    /// At startup seeds are dialed while fewer entries than this were ever reached.
    pub const BOOTSTRAP_REACHED: u64 = 4;
    /// Wall-clock bound of one dial or address request.
    pub const OPERATION_TIME: Duration = Duration::from_secs(10);

    /// `path` is where the book is saved after a round changes it.
    pub fn new(network: Arc<PeerNetwork>, book: Arc<Mutex<PeerBook>>, path: PathBuf) -> Self {
        Self {
            network,
            book,
            path,
            round: AtomicU64::new(0),
            seed_error: Mutex::new(None),
            gossip_error: Mutex::new(None),
            sync_backoff: Mutex::new(Vec::new()),
            own_addresses: Mutex::new(Vec::new()),
        }
    }

    /// Peers inventory sync is holding off (evicted, idle or blamed): neither
    /// seed passes nor candidate dials reconnect them until they leave this set.
    pub fn hold_off(&self, peers: Vec<String>) -> Result<(), Error> {
        *self.sync_backoff.lock().map_err(|_| poisoned())? = peers;
        Ok(())
    }

    fn lock(&self) -> Result<MutexGuard<'_, PeerBook>, Error> {
        self.book.lock().map_err(|_| poisoned())
    }

    /// One bounded round: merge peers learned through identify, refresh the
    /// served gossip snapshot, seeds when due, up to two dials while below
    /// target, every thirtieth round one address request.
    pub fn step(&self, engine: &Engine, stop: &AtomicBool) -> Result<(), Error> {
        let round = self.round.fetch_add(1, Ordering::Relaxed);
        let omitting = engine.omitting_peers()?;
        let mut changed = self.merge_identified(&omitting)?;
        self.publish(&omitting)?;
        let mut budget = Budget {
            left: Self::OPERATIONS_PER_ROUND,
            stop,
        };
        let mut avoid = omitting.clone();
        avoid.extend(
            self.sync_backoff
                .lock()
                .map_err(|_| poisoned())?
                .iter()
                .cloned(),
        );
        if self.seeds_due(round)? {
            changed |= self.dial_seeds(&avoid, &mut budget, round == 0)?;
        }
        changed |= self.dial_candidates(&avoid, &mut budget)?;
        if round % Self::GOSSIP_EVERY == Self::GOSSIP_EVERY - 1 {
            changed |= self.ask_for_peers(&omitting, &mut budget)?;
        }
        if changed {
            self.lock()?.save(&self.path)?;
        }
        Ok(())
    }

    /// Peers that identified themselves to us, with public literal addresses,
    /// enter the book as gossip under the same caps; a local operation.
    fn merge_identified(&self, omitting: &[String]) -> Result<bool, Error> {
        let at = now()?;
        let peers: Vec<GossipPeer> = self
            .network
            .drain_identified()
            .into_iter()
            .map(|(peer_id, addrs)| GossipPeer {
                peer_id,
                addrs,
                last_seen: at,
            })
            .collect();
        if peers.is_empty() {
            return Ok(false);
        }
        let local = self.network.peer_id();
        Ok(self.lock()?.merge_identified(&peers, local, omitting) > 0)
    }

    /// Verified book entries; this node itself is learned through identify.
    fn publish(&self, omitting: &[String]) -> Result<(), Error> {
        let limit = usize::try_from(Self::GOSSIP_LIMIT)
            .map_err(|error| Error::Invalid(format!("gossip limit: {error}")))?;
        let at = now()?;
        let mut peers = Vec::new();
        for mut peer in self.lock()?.gossip(limit, omitting) {
            peer.last_seen = peer.last_seen.min(at);
            peers.push(peer);
        }
        self.network.set_gossip(peers);
        Ok(())
    }

    /// Isolated nodes retry seeds every round, limited by each seed's backoff.
    fn seeds_due(&self, round: u64) -> Result<bool, Error> {
        let connected = self.network.connected_peer_ids().len();
        if connected >= Self::TARGET {
            return Ok(false);
        }
        if connected == 0 {
            return Ok(true);
        }
        let reached_ever = self.lock()?.stats().reached_ever;
        Ok((round == 0 && reached_ever < Self::BOOTSTRAP_REACHED)
            || (round > 0 && round % Self::SEED_EVERY == 0))
    }

    /// Dial one address outside the book lock and record the outcome. An
    /// address that reached this node itself (a seed resolving to us) is
    /// remembered and never dialed again by this process; the book keeps it,
    /// since the same name is a real peer for every other node.
    fn dial(&self, address: &Multiaddr, stop: &AtomicBool) -> Result<Result<PeerId, Error>, Error> {
        let deadline = Instant::now() + Self::OPERATION_TIME;
        let local = Protocol::P2p(self.network.peer_id());
        let outcome = match address.iter().any(|part| part == local) {
            true => Err(Error::LocalPeer),
            false => self.network.dial(address.clone(), stop, deadline),
        };
        if stop.load(Ordering::Acquire) {
            return Ok(outcome);
        }
        let at = now()?;
        let mut book = self.lock()?;
        match &outcome {
            Ok(peer) => book.succeeded(address, *peer, self.network.peer_id(), at),
            Err(Error::LocalPeer) if names_self(address, &local) => {
                self.own()?.push(address.clone())
            }
            // A name may resolve to other hosts too: skip only this dial.
            Err(Error::LocalPeer) => (),
            // Our own capacity or a busy peer says nothing about the address.
            Err(Error::DialDenied(_) | Error::Busy(_) | Error::PeerBusy) => (),
            Err(_failure) => book.failed(address, at),
        }
        Ok(outcome)
    }

    fn own(&self) -> Result<MutexGuard<'_, Vec<Multiaddr>>, Error> {
        self.own_addresses.lock().map_err(|_| poisoned())
    }

    /// The startup pass ignores backoff persisted by an earlier process.
    fn dial_seeds(
        &self,
        omitting: &[String],
        budget: &mut Budget,
        startup: bool,
    ) -> Result<bool, Error> {
        let backoff_at = if startup { None } else { Some(now()?) };
        let mut seeds = self.lock()?.seeds(omitting, backoff_at);
        let own = self.own()?.clone();
        seeds.retain(|seed| !own.contains(seed));
        let mut dialed = false;
        let mut error = None;
        let mut reached = false;
        for seed in seeds {
            if !budget.take() {
                break;
            }
            match self.dial(&seed, budget.stop)? {
                Ok(_peer) => reached = true,
                Err(Error::LocalPeer) => continue,
                Err(failure) => error = Some(format!("{seed}: {failure}")),
            }
            dialed = true;
        }
        if dialed {
            let recorded = if reached {
                None
            } else {
                error.map(RecordedError::now)
            };
            *self.seed_error.lock().map_err(|_| poisoned())? = recorded;
        }
        Ok(dialed)
    }

    fn dial_candidates(&self, omitting: &[String], budget: &mut Budget) -> Result<bool, Error> {
        let connected = self.network.connected_peer_ids();
        if connected.len() >= Self::TARGET {
            return Ok(false);
        }
        let mut candidates =
            self.lock()?
                .candidates(now()?, &connected, omitting, Self::DIALS_PER_ROUND);
        let own = self.own()?.clone();
        candidates.retain(|candidate| !own.contains(candidate));
        let mut dialed = false;
        for candidate in candidates {
            if !budget.take() {
                break;
            }
            dialed = true;
            // Either outcome is already recorded in the book (success or backoff).
            match self.dial(&candidate, budget.stop)? {
                Ok(_reached) => (),
                Err(_backed_off) => (),
            }
        }
        Ok(dialed)
    }

    /// Ask one random connected, non-omitting peer for addresses and merge
    /// them under the book's caps. Lock order: the book lock is released
    /// before `gossip_error` is taken; the two are never held together.
    fn ask_for_peers(&self, omitting: &[String], budget: &mut Budget) -> Result<bool, Error> {
        let connected: Vec<PeerId> = self
            .network
            .connected_peer_ids()
            .into_iter()
            .filter(|peer| !omitting.contains(&peer.to_string()))
            .collect();
        let Some(peer) = connected.choose(&mut rand::thread_rng()).copied() else {
            return Ok(false);
        };
        if !budget.take() {
            return Ok(false);
        }
        let deadline = Instant::now() + Self::OPERATION_TIME;
        let outcome = self
            .network
            .peers_of(peer, Self::GOSSIP_LIMIT, budget.stop, deadline);
        let (changed, recorded) = match outcome {
            Ok(peers) => {
                let local = self.network.peer_id();
                (self.lock()?.merge(&peers, local, omitting) > 0, None)
            }
            Err(failure) => (
                false,
                Some(RecordedError::now(format!("{peer}: {failure}"))),
            ),
        };
        *self.gossip_error.lock().map_err(|_| poisoned())? = recorded;
        Ok(changed)
    }

    pub fn status(&self) -> Result<DiscoveryStatus, Error> {
        let stats = self.lock()?.stats();
        let (last_seed_round_error, last_seed_round_error_at) =
            RecordedError::split(read(&self.seed_error, "seed round")?);
        let (last_gossip_error, last_gossip_error_at) =
            RecordedError::split(read(&self.gossip_error, "gossip")?);
        let mut status = DiscoveryStatus::default();
        status.seeds = stats.seeds;
        status.book = stats.book;
        status.reached = stats.reached;
        status.reached_ever = stats.reached_ever;
        status.gossip = stats.gossip;
        status.last_seed_round_error = last_seed_round_error;
        status.last_seed_round_error_at = last_seed_round_error_at;
        status.last_gossip_error = last_gossip_error;
        status.last_gossip_error_at = last_gossip_error_at;
        Ok(status)
    }

    /// Peers the inventory scheduler caught misbehaving back off in the book
    /// too, so discovery stops dialing them; the book is saved if it changed.
    pub fn penalize(&self, peers: &[PeerId]) -> Result<(), Error> {
        if peers.is_empty() {
            return Ok(());
        }
        let at = now()?;
        let mut book = self.lock()?;
        let mut changed = false;
        for peer in peers {
            changed |= book.failed_peer(*peer, at);
        }
        if changed {
            book.save(&self.path)?;
        }
        Ok(())
    }

    pub fn book(&self) -> Result<Vec<BookEntry>, Error> {
        Ok(self.lock()?.listing())
    }
}
