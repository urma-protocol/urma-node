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
use std::collections::{HashMap, HashSet};

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
    witnesses: HashMap<Multiaddr, HashSet<PeerId>>,
    retried: HashSet<Multiaddr>,
}

fn is_quic(address: &Multiaddr) -> bool {
    address.iter().any(|part| part == Protocol::QuicV1)
}

impl Externals {
    const WITNESSED_MAX: usize = 16;
    const MOVED: usize = 2;

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

    /// `witness`, a peer we dialed directly, sees us at `observed`. Returns
    /// the confirmed addresses this shows stale.
    pub(crate) fn witnessed(
        &mut self,
        observed: &Multiaddr,
        witness: PeerId,
        externals: &[Multiaddr],
    ) -> Vec<Multiaddr> {
        let full = self.witnesses.len() >= Self::WITNESSED_MAX;
        if full && !self.witnesses.contains_key(observed) {
            if let Some(evicted) = self.witnesses.keys().next().cloned() {
                self.witnesses.remove(&evicted);
            }
        }
        let seen = self.witnesses.entry(observed.clone()).or_default();
        if !seen.insert(witness) || seen.len() < Self::MOVED || self.announced() {
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
            .is_some_and(|seen| seen.contains(&server));
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

    #[test]
    fn a_witness_cannot_confirm_what_it_reported_and_gets_one_retry_elsewhere() {
        let (local, liar, honest) = (peer(), peer(), peer());
        let mut externals = Externals::new(local, Vec::new());
        let victim = address("/ip4/1.2.3.4/tcp/28332");
        externals.witnessed(&victim, liar, &[]);
        let retry = victim.clone().with(Protocol::P2p(local));
        assert_eq!(
            externals.verdict(&victim, liar),
            Verdict::Retry(retry.clone())
        );
        assert_eq!(externals.verdict(&retry, liar), Verdict::Ignore);
        assert_eq!(externals.verdict(&retry, honest), Verdict::Confirm(victim));
    }

    #[test]
    fn a_bound_address_counts_whoever_confirms_it() {
        let (local, seed) = (peer(), peer());
        let mut externals = Externals::new(local, Vec::new());
        let listener = address("/ip4/1.2.3.4/tcp/28332");
        assert_eq!(externals.bound(&listener), Some(listener.clone()));
        externals.witnessed(&listener, seed, &[]);
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
        externals.witnessed(&other, a, &[announce.clone()]);
        assert!(externals.witnessed(&other, b, &[announce]).is_empty());
    }

    #[test]
    fn two_peers_seeing_another_address_retire_a_confirmed_one_of_that_transport() {
        let mut externals = Externals::new(peer(), Vec::new());
        let old = address("/ip4/1.2.3.4/tcp/40000");
        let quic = address("/ip4/1.2.3.4/udp/40000/quic-v1");
        let moved = address("/ip4/1.2.3.4/tcp/40001");
        let confirmed = [old.clone(), quic];
        let (a, b) = (peer(), peer());
        assert!(externals.witnessed(&moved, a, &confirmed).is_empty());
        assert!(externals.witnessed(&moved, a, &confirmed).is_empty());
        assert_eq!(externals.witnessed(&moved, b, &confirmed), vec![old]);
    }

    #[test]
    fn a_bound_address_never_goes_stale() {
        let mut externals = Externals::new(peer(), Vec::new());
        let listener = address("/ip4/1.2.3.4/tcp/28332");
        externals.bound(&listener);
        let moved = address("/ip4/1.2.3.5/tcp/28332");
        let (a, b) = (peer(), peer());
        externals.witnessed(&moved, a, &[listener.clone()]);
        assert!(externals.witnessed(&moved, b, &[listener]).is_empty());
    }
}
