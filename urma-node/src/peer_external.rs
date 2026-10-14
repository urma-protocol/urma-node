//! Which addresses this node advertises as its own (URMA_NODE_NAT.md §3.2).
//! With `--urma-announce` they are exactly the announced ones. Otherwise an
//! AutoNAT confirmation counts only when the confirming server did not itself
//! report the address: a peer could otherwise name any victim address and
//! "confirm" it. Addresses this node is bound to are exempt, since no peer can
//! make us bind an IP. libp2p's client tests an address once, so a rejected
//! confirmation is retried once under the equivalent `/p2p/<self>` form. If
//! the retry also lands on the witness the address stays unconfirmed: an
//! address only one witness ever reports and tests is unconfirmable by design.
//! A confirmed address goes stale when two peers we dialed both see us at one
//! other address of the same transport and IP family: the mapping changed.
//! Reports are kept per witness: a peer reporting more than a handful of
//! addresses is flooding, its reports stop counting and it confirms nothing.
use crate::peer_reach::{direct_shape, without_peer};
use libp2p::{Multiaddr, PeerId, multiaddr::Protocol};
use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

/// One witness: when we first heard from it and what it reported when.
struct Witness {
    first: Instant,
    reports: Vec<(Multiaddr, Instant)>,
    flooded: bool,
}

impl Witness {
    fn reported(&self, address: &Multiaddr) -> bool {
        self.flooded || self.reports.iter().any(|(known, _)| known == address)
    }

    fn recently(&self, address: &Multiaddr, now: Instant) -> bool {
        !self.flooded
            && self
                .reports
                .iter()
                .any(|(known, at)| known == address && now.duration_since(*at) <= Externals::WINDOW)
    }
}

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
    witnesses: HashMap<PeerId, Witness>,
    retried: HashSet<Multiaddr>,
}

fn is_quic(address: &Multiaddr) -> bool {
    address.iter().any(|part| part == Protocol::QuicV1)
}

fn is_ipv4(address: &Multiaddr) -> bool {
    matches!(address.iter().next(), Some(Protocol::Ip4(_)))
}

/// Same transport and IP family: one NAT mapping.
fn same_mapping(a: &Multiaddr, b: &Multiaddr) -> bool {
    is_quic(a) == is_quic(b) && is_ipv4(a) == is_ipv4(b)
}

impl Externals {
    const REPORTS_MAX: usize = 4;
    const WITNESSES_MAX: usize = 256;
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

    /// Whether the witness reported more addresses than `REPORTS_MAX`: its
    /// reports are no AutoNAT candidates and count toward nothing.
    pub(crate) fn flooded(&self, witness: PeerId) -> bool {
        self.witnesses
            .get(&witness)
            .is_some_and(|known| known.flooded)
    }

    /// Records the report under its witness. A new witness past
    /// `WITNESSES_MAX` evicts the one first heard longest ago.
    fn record(&mut self, observed: &Multiaddr, witness: PeerId, now: Instant) {
        if !self.witnesses.contains_key(&witness) && self.witnesses.len() >= Self::WITNESSES_MAX {
            let oldest = self
                .witnesses
                .iter()
                .min_by_key(|(_, known)| known.first)
                .map(|(peer, _)| *peer);
            if let Some(oldest) = oldest {
                self.witnesses.remove(&oldest);
            }
        }
        let entry = self.witnesses.entry(witness).or_insert(Witness {
            first: now,
            reports: Vec::new(),
            flooded: false,
        });
        if entry.flooded {
            return;
        }
        if let Some(known) = entry
            .reports
            .iter_mut()
            .find(|(known, _)| known == observed)
        {
            known.1 = now;
        } else if entry.reports.len() >= Self::REPORTS_MAX {
            entry.flooded = true;
            entry.reports.clear();
        } else {
            entry.reports.push((observed.clone(), now));
        }
    }

    /// `witness`, a peer we dialed directly, sees us at `observed` at `now`.
    /// Returns the confirmed addresses this shows stale: two witnesses within
    /// the window, both still holding a data connection (`data`), agree on
    /// another address of the same transport and IP family.
    pub(crate) fn witnessed(
        &mut self,
        observed: &Multiaddr,
        (witness, now): (PeerId, Instant),
        externals: &[Multiaddr],
        data: impl Fn(&PeerId) -> bool,
    ) -> Vec<Multiaddr> {
        self.record(observed, witness, now);
        let recent = self
            .witnesses
            .iter()
            .filter(|(peer, known)| known.recently(observed, now) && data(peer))
            .count();
        if recent < Self::MOVED || self.announced() {
            return Vec::new();
        }
        externals
            .iter()
            .filter(|known| {
                *known != observed && same_mapping(known, observed) && !self.bound.contains(*known)
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
            .get(&server)
            .is_some_and(|known| known.reported(&bare));
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
    fn a_liar_cannot_flush_its_own_report_before_confirming_it() {
        let (local, liar, honest) = (peer(), peer(), peer());
        let mut externals = Externals::new(local, Vec::new());
        let victim = address("/ip4/1.2.3.4/tcp/28332");
        saw(&mut externals, &victim, liar, &[]);
        for port in 1..=Externals::REPORTS_MAX {
            let junk = address(&format!("/ip4/9.9.9.9/tcp/{port}"));
            saw(&mut externals, &junk, liar, &[]);
        }
        assert!(externals.flooded(liar));
        assert_eq!(
            externals.verdict(&victim, liar),
            Verdict::Retry(victim.clone().with(Protocol::P2p(local)))
        );
        let honest_victim = address("/ip4/5.6.7.8/tcp/28332");
        assert_eq!(
            externals.verdict(&honest_victim, liar),
            Verdict::Retry(honest_victim.clone().with(Protocol::P2p(local))),
            "a flooding witness confirms nothing"
        );
        assert_eq!(externals.verdict(&victim, honest), Verdict::Confirm(victim));
    }

    #[test]
    fn a_flooding_witness_counts_toward_no_moved_mapping() {
        let mut externals = Externals::new(peer(), Vec::new());
        let old = address("/ip4/1.2.3.4/tcp/40000");
        let moved = address("/ip4/1.2.3.4/tcp/40001");
        let (liar, honest) = (peer(), peer());
        for port in 1..=Externals::REPORTS_MAX {
            saw(
                &mut externals,
                &address(&format!("/ip4/9.9.9.9/tcp/{port}")),
                liar,
                &[],
            );
        }
        saw(&mut externals, &moved, liar, &[old.clone()]);
        assert!(saw(&mut externals, &moved, honest, &[old]).is_empty());
    }

    #[test]
    fn a_full_table_evicts_the_witness_heard_first() {
        let mut externals = Externals::new(peer(), Vec::new());
        let start = Instant::now();
        let first = peer();
        let observed = address("/ip4/1.2.3.4/tcp/40001");
        externals.witnessed(&observed, (first, start), &[], |_| true);
        for n in 1..Externals::WITNESSES_MAX {
            let at = start + Duration::from_millis(u64::try_from(n).unwrap());
            externals.witnessed(&observed, (peer(), at), &[], |_| true);
        }
        assert!(externals.witnesses.contains_key(&first));
        externals.witnessed(
            &observed,
            (peer(), start + Duration::from_secs(1)),
            &[],
            |_| true,
        );
        assert_eq!(externals.witnesses.len(), Externals::WITNESSES_MAX);
        assert!(!externals.witnesses.contains_key(&first));
    }

    #[test]
    fn a_moved_mapping_retires_only_the_same_ip_family() {
        let mut externals = Externals::new(peer(), Vec::new());
        let v4 = address("/ip4/1.2.3.4/tcp/40000");
        let v6 = address("/ip6/2001:db8::1/tcp/40000");
        let moved = address("/ip4/1.2.3.4/tcp/40001");
        let confirmed = [v4.clone(), v6];
        let (a, b) = (peer(), peer());
        saw(&mut externals, &moved, a, &confirmed);
        assert_eq!(saw(&mut externals, &moved, b, &confirmed), vec![v4]);
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
