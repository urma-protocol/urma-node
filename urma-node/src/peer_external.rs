//! Which addresses this node advertises as its own (URMA_NODE_NAT.md §3.2).
//! With `--urma-announce` they are exactly the announced ones. Otherwise an
//! AutoNAT confirmation counts only when the confirming server did not itself
//! report the address: a peer could otherwise name any victim address and
//! "confirm" it. Addresses this node is bound to are exempt, since no peer can
//! make us bind an IP. libp2p's client tests an address once, so a rejected
//! confirmation is retried once under the equivalent `/p2p/<self>` form.
//! A confirmed address goes stale when two peers we dialed both see us at one
//! other address of the same transport: the mapping changed.
use crate::peer_reach::{direct_shape, without_peer};
use libp2p::{Multiaddr, PeerId, multiaddr::Protocol};
use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

/// What one successful AutoNAT result means for our advertised addresses.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    Confirm(Multiaddr),
    Retry(Multiaddr),
    Ignore,
}

pub(crate) struct Externals {
    local: PeerId,
    announce: Vec<Multiaddr>,
    bound: HashSet<Multiaddr>,
    witnesses: HashMap<Multiaddr, HashMap<PeerId, Instant>>,
    retried: HashSet<Multiaddr>,
}

fn is_quic(address: &Multiaddr) -> bool {
    address.iter().any(|part| part == Protocol::QuicV1)
}

impl Externals {
    const WITNESSED_MAX: usize = 16;
    const MOVED: usize = 2;
    const WINDOW: Duration = Duration::from_secs(10 * 60);

    pub(crate) fn new(local: PeerId, announce: Vec<Multiaddr>) -> Self {
        Self {
            local,
            announce,
            bound: HashSet::new(),
            witnesses: HashMap::new(),
            retried: HashSet::new(),
        }
    }

    pub(crate) fn announced(&self) -> bool {
        !self.announce.is_empty()
    }

    /// Whether a confirmed direct address may be advertised.
    pub(crate) fn admits(&self, address: &Multiaddr) -> bool {
        !self.announced() || self.announce.contains(address)
    }

    /// A public address we listen on; returns it as an AutoNAT candidate.
    pub(crate) fn bound(&mut self, address: &Multiaddr) -> Option<Multiaddr> {
        let bare = without_peer(address);
        if !direct_shape(&bare) {
            return None;
        }
        self.bound.insert(bare.clone());
        Some(bare)
    }

    /// `witness`, a peer we dialed directly, sees us at `observed` at `now`.
    /// Returns the confirmed addresses this shows stale: two witnesses within
    /// the window, both still holding a data connection (`data`), agree on
    /// another address of the same transport.
    pub(crate) fn witnessed(
        &mut self,
        observed: &Multiaddr,
        (witness, now): (PeerId, Instant),
        externals: &[Multiaddr],
        data: impl Fn(&PeerId) -> bool,
    ) -> Vec<Multiaddr> {
        let full = self.witnesses.len() >= Self::WITNESSED_MAX;
        if full && !self.witnesses.contains_key(observed) {
            if let Some(evicted) = self.witnesses.keys().next().cloned() {
                self.witnesses.remove(&evicted);
            }
        }
        let seen = self.witnesses.entry(observed.clone()).or_default();
        seen.insert(witness, now);
        let recent = seen
            .iter()
            .filter(|(peer, at)| now.duration_since(**at) <= Self::WINDOW && data(peer))
            .count();
        if recent < Self::MOVED || self.announced() {
            return Vec::new();
        }
        externals
            .iter()
            .filter(|known| {
                *known != observed
                    && is_quic(known) == is_quic(observed)
                    && !self.bound.contains(*known)
            })
            .cloned()
            .collect()
    }

    pub(crate) fn verdict(&mut self, tested: &Multiaddr, server: PeerId) -> Verdict {
        let bare = without_peer(tested);
        if self.announced() {
            return Verdict::Ignore;
        }
        let witnessed = self
            .witnesses
            .get(&bare)
            .is_some_and(|seen| seen.contains_key(&server));
        if !witnessed || self.bound.contains(&bare) {
            return Verdict::Confirm(bare);
        }
        match self.retried.insert(bare.clone()) {
            true => Verdict::Retry(bare.with(Protocol::P2p(self.local))),
            false => Verdict::Ignore,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libp2p::identity::Keypair;

    fn peer() -> PeerId {
        Keypair::generate_ed25519().public().to_peer_id()
    }

    fn address(text: &str) -> Multiaddr {
        text.parse().unwrap()
    }

    /// `witness` reports `observed` now, while every witness holds data.
    fn saw(
        externals: &mut Externals,
        observed: &Multiaddr,
        witness: PeerId,
        confirmed: &[Multiaddr],
    ) -> Vec<Multiaddr> {
        externals.witnessed(observed, (witness, Instant::now()), confirmed, |_| true)
    }

    #[test]
    fn a_witness_cannot_confirm_what_it_reported_and_gets_one_retry_elsewhere() {
        let (local, liar, honest) = (peer(), peer(), peer());
        let mut externals = Externals::new(local, Vec::new());
        let victim = address("/ip4/1.2.3.4/tcp/28332");
        saw(&mut externals, &victim, liar, &[]);
        let retry = victim.clone().with(Protocol::P2p(local));
        assert_eq!(
            externals.verdict(&victim, liar),
            Verdict::Retry(retry.clone())
        );
        assert_eq!(externals.verdict(&retry, liar), Verdict::Ignore);
        assert_eq!(externals.verdict(&retry, honest), Verdict::Confirm(victim));
    }

    #[test]
    fn a_retried_confirmation_is_advertised_without_its_p2p_suffix() {
        let (local, witness, other) = (peer(), peer(), peer());
        let mut externals = Externals::new(local, Vec::new());
        let mapped = address("/ip4/1.2.3.4/udp/28332/quic-v1");
        saw(&mut externals, &mapped, witness, &[]);
        let Verdict::Retry(retry) = externals.verdict(&mapped, witness) else {
            panic!("a witness confirmation is retried");
        };
        let Verdict::Confirm(advertised) = externals.verdict(&retry, other) else {
            panic!("another server confirms the retry");
        };
        assert_eq!(advertised, mapped);
        assert!(
            advertised
                .iter()
                .all(|part| !matches!(part, Protocol::P2p(_)))
        );
    }

    #[test]
    fn a_bound_address_counts_whoever_confirms_it() {
        let (local, seed) = (peer(), peer());
        let mut externals = Externals::new(local, Vec::new());
        let listener = address("/ip4/1.2.3.4/tcp/28332");
        assert_eq!(externals.bound(&listener), Some(listener.clone()));
        saw(&mut externals, &listener, seed, &[]);
        assert_eq!(
            externals.verdict(&listener, seed),
            Verdict::Confirm(listener)
        );
        assert_eq!(externals.bound(&address("/ip4/10.0.0.1/tcp/1")), None);
    }

    #[test]
    fn announced_addresses_are_the_only_externals() {
        let announce = address("/ip4/1.2.3.4/tcp/28332");
        let mut externals = Externals::new(peer(), vec![announce.clone()]);
        let other = address("/ip4/5.6.7.8/udp/28332/quic-v1");
        assert_eq!(externals.verdict(&other, peer()), Verdict::Ignore);
        assert!(externals.admits(&announce));
        assert!(!externals.admits(&other));
        let (a, b) = (peer(), peer());
        saw(&mut externals, &other, a, &[announce.clone()]);
        assert!(saw(&mut externals, &other, b, &[announce]).is_empty());
    }

    #[test]
    fn two_peers_seeing_another_address_retire_a_confirmed_one_of_that_transport() {
        let mut externals = Externals::new(peer(), Vec::new());
        let old = address("/ip4/1.2.3.4/tcp/40000");
        let quic = address("/ip4/1.2.3.4/udp/40000/quic-v1");
        let moved = address("/ip4/1.2.3.4/tcp/40001");
        let confirmed = [old.clone(), quic];
        let (a, b) = (peer(), peer());
        assert!(saw(&mut externals, &moved, a, &confirmed).is_empty());
        assert!(saw(&mut externals, &moved, a, &confirmed).is_empty());
        assert_eq!(saw(&mut externals, &moved, b, &confirmed), vec![old]);
    }

    #[test]
    fn only_recent_reports_from_peers_still_holding_data_count() {
        let mut externals = Externals::new(peer(), Vec::new());
        let old = address("/ip4/1.2.3.4/tcp/40000");
        let moved = address("/ip4/1.2.3.4/tcp/40001");
        let (a, b, c) = (peer(), peer(), peer());
        let start = Instant::now();
        let later = start + Externals::WINDOW + Duration::from_secs(1);
        let confirmed = [old.clone()];
        externals.witnessed(&moved, (a, start), &confirmed, |_| true);
        let late = externals.witnessed(&moved, (b, later), &confirmed, |_| true);
        assert!(late.is_empty(), "a's report fell out of the window");
        let gone = externals.witnessed(&moved, (c, later), &confirmed, |peer| *peer != b);
        assert!(gone.is_empty(), "b no longer holds a data connection");
        let both = externals.witnessed(&moved, (c, later), &confirmed, |_| true);
        assert_eq!(both, vec![old]);
    }

    #[test]
    fn a_bound_address_never_goes_stale() {
        let mut externals = Externals::new(peer(), Vec::new());
        let listener = address("/ip4/1.2.3.4/tcp/28332");
        externals.bound(&listener);
        let moved = address("/ip4/1.2.3.5/tcp/28332");
        let (a, b) = (peer(), peer());
        saw(&mut externals, &moved, a, &[listener.clone()]);
        assert!(saw(&mut externals, &moved, b, &[listener]).is_empty());
    }
}
