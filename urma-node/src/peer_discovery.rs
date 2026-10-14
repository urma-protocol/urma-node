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
    peer_address::Dialed,
    peer_protocol::GossipPeer,
    peerbook::PeerBook,
    recorded::{RecordedError, read, unix_seconds as now},
};
use libp2p::{Multiaddr, PeerId, multiaddr::Protocol};
use rand::seq::SliceRandom;
use std::{
    mem,
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
    relayed: Mutex<Vec<Relayed>>,
}

/// A circuit dial that reached its peer only through a relay. It is neither
/// reached nor failed until DCUtR gives the peer a direct connection.
struct Relayed {
    address: Multiaddr,
    peer: PeerId,
    since: Instant,
}

/// What a waiting circuit dial needs this round.
#[derive(Debug, PartialEq, Eq)]
enum Settle {
    /// A direct connection exists: run the chain probe over it.
    Probe,
    /// Still relayed within the circuit lifetime: DCUtR may yet succeed.
    Wait,
    /// The relayed contact ended, or outlived the circuit, without a direct
    /// connection: hole punching failed, so the address backs off.
    Fail,
}

fn settle(direct: bool, connected: bool, waited: Duration) -> Settle {
    match (direct, connected && waited < Discovery::CIRCUIT_WAIT) {
        (true, _) => Settle::Probe,
        (false, true) => Settle::Wait,
        (false, false) => Settle::Fail,
    }
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

/// A seed already connected (relayed connections included) is never
/// re-dialed: the dial would only re-run the chain probe over a live link.
fn names_connected(address: &Multiaddr, connected: &[PeerId]) -> bool {
    matches!(address.iter().last(), Some(Protocol::P2p(peer)) if connected.contains(&peer))
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
    /// How long a relayed-only contact may wait for DCUtR: one circuit.
    pub const CIRCUIT_WAIT: Duration = Duration::from_secs(120);
    /// Circuit dials waiting for a direct connection at once.
    pub const RELAYED_MAX: usize = 16;

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
            relayed: Mutex::new(Vec::new()),
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
        changed |= self.settle_relayed(&mut budget)?;
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
        let connected = self.direct_peers().len();
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
            false => match self.network.dns().admits(address) {
                // Without a resolver the address is unreachable here, not bad.
                Err(unresolvable) => return Ok(Err(unresolvable)),
                Ok(()) => match self.network.dial_any(address.clone(), stop, deadline) {
                    Ok(Dialed::Direct(peer)) => Ok(peer),
                    // Neither reached nor failed: Busy leaves the book as is.
                    Ok(Dialed::Relayed(peer)) => {
                        self.await_direct(address.clone(), peer)?;
                        Err(Error::Busy(format!(
                            "{peer} reachable only through a relay; awaiting hole punching"
                        )))
                    }
                    Err(failure) => Err(failure),
                },
            },
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
        let connected = self.network.connected_peer_ids();
        seeds.retain(|seed| !own.contains(seed) && !names_connected(seed, &connected));
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
        if self.direct_peers().len() >= Self::TARGET {
            return Ok(false);
        }
        // Relayed-only peers are excluded too: their circuit is already open.
        let connected = self.network.connected_peer_ids();
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
            .filter(|peer| self.network.can_gossip(*peer))
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

    /// Peers holding a direct data connection. Only these count toward the
    /// target and carry records/5 (gossip, chain probe); a relayed-only or
    /// auxiliary-only peer doesn't.
    fn direct_peers(&self) -> Vec<PeerId> {
        let mut peers = self.network.connected_peer_ids();
        peers.retain(|peer| self.network.has_direct(*peer));
        peers
    }

    fn relayed(&self) -> Result<MutexGuard<'_, Vec<Relayed>>, Error> {
        self.relayed.lock().map_err(|_| poisoned())
    }

    /// Remember a circuit dial until DCUtR settles it; a newer contact
    /// replaces the peer's older one. The oldest of a full list makes room
    /// and counts as a failed dial; the book lock is taken only after the
    /// waiting list is released.
    fn await_direct(&self, address: Multiaddr, peer: PeerId) -> Result<(), Error> {
        let evicted = {
            let mut relayed = self.relayed()?;
            relayed.retain(|waiting| waiting.peer != peer);
            let evicted = match relayed.len() == Self::RELAYED_MAX {
                true => Some(relayed.remove(0)),
                false => None,
            };
            relayed.push(Relayed {
                address,
                peer,
                since: Instant::now(),
            });
            evicted
        };
        let Some(entry) = evicted else {
            return Ok(());
        };
        self.lock()?.failed(&entry.address, now()?);
        Ok(())
    }

    /// Settle circuit dials (contract §3.6): a peer that gained a direct
    /// connection gets the chain probe over it and counts as reached; one
    /// whose relayed contact ended or outlived the circuit backs off. The
    /// waiting list and the book lock are never held together.
    fn settle_relayed(&self, budget: &mut Budget) -> Result<bool, Error> {
        let waiting = mem::take(&mut *self.relayed()?);
        let connected = self.network.connected_peer_ids();
        let mut keep = Vec::new();
        let mut changed = false;
        for entry in waiting {
            let direct = self.network.has_direct(entry.peer);
            let verdict = settle(
                direct,
                connected.contains(&entry.peer),
                entry.since.elapsed(),
            );
            let settled = match verdict {
                Settle::Wait => false,
                Settle::Probe => budget.take() && self.probe_relayed(&entry, budget.stop)?,
                Settle::Fail => {
                    self.lock()?.failed(&entry.address, now()?);
                    true
                }
            };
            match settled {
                true => changed = true,
                false => keep.push(entry),
            }
        }
        self.relayed()?.extend(keep);
        Ok(changed)
    }

    /// The chain probe over a punched connection; false while the peer is
    /// busy or the request found no data connection yet (Transport), so the
    /// contact waits, at most until the circuit expires.
    fn probe_relayed(&self, entry: &Relayed, stop: &AtomicBool) -> Result<bool, Error> {
        let deadline = Instant::now() + Self::OPERATION_TIME;
        let outcome = self.network.probe(entry.peer, stop, deadline);
        let at = now()?;
        let local = self.network.peer_id();
        match outcome {
            Ok(peer) => self.lock()?.succeeded(&entry.address, peer, local, at),
            Err(Error::Busy(_) | Error::PeerBusy | Error::Transport(_)) => return Ok(false),
            Err(_failure) => self.lock()?.failed(&entry.address, at),
        }
        Ok(true)
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

#[cfg(test)]
mod tests {
    use super::{Discovery, Settle, names_connected, settle};
    use crate::{Error, error::DialLimit, peer_gossip::probe_closes};
    use libp2p::{Multiaddr, PeerId, identity::Keypair, multiaddr::Protocol};
    use std::time::Duration;

    fn peer() -> PeerId {
        Keypair::generate_ed25519().public().to_peer_id()
    }

    #[test]
    fn only_a_peer_on_another_chain_loses_its_connections_to_a_failed_probe() {
        assert!(probe_closes(&Error::Invalid(
            "peer response network mismatch".into()
        )));
        let kept = [
            Error::Busy("peer dial deadline".into()),
            Error::PeerBusy,
            Error::Transport("Timeout".into()),
            Error::Transport("Failed to dial the requested peer".into()),
            Error::DialDenied(DialLimit::Duplicate),
        ];
        for error in kept {
            assert!(!probe_closes(&error), "{error}");
        }
    }

    #[test]
    fn a_connected_seed_is_not_dialed_again() {
        let (seed, other) = (peer(), peer());
        let named: Multiaddr = format!("/ip4/11.0.0.1/tcp/28332/p2p/{seed}")
            .parse()
            .unwrap();
        assert!(names_connected(&named, &[other, seed]));
        assert!(!names_connected(&named, &[other]));
        let unnamed: Multiaddr = "/dnsaddr/seed.example".parse().unwrap();
        assert!(
            !names_connected(&unnamed, &[seed]),
            "an unidentified seed is dialed"
        );
        let circuit = named
            .clone()
            .with(Protocol::P2pCircuit)
            .with(Protocol::P2p(other));
        assert!(names_connected(&circuit, &[other]));
    }

    #[test]
    fn a_punched_peer_is_probed_whatever_its_relay_does() {
        let late = Discovery::CIRCUIT_WAIT + Duration::from_secs(1);
        assert_eq!(settle(true, true, Duration::ZERO), Settle::Probe);
        assert_eq!(settle(true, false, late), Settle::Probe);
    }

    #[test]
    fn a_relayed_contact_waits_one_circuit_then_fails() {
        let early = Discovery::CIRCUIT_WAIT - Duration::from_secs(1);
        assert_eq!(settle(false, true, early), Settle::Wait);
        assert_eq!(settle(false, true, Discovery::CIRCUIT_WAIT), Settle::Fail);
        assert_eq!(settle(false, false, Duration::ZERO), Settle::Fail);
    }
}
