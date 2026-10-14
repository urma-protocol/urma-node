//! Per-peer connection counts and the link they amount to: direct only with a
//! data connection, relayed only when every connection rides a circuit.
use crate::{peer_limits::Class, peer_nat::PeerLink};

/// Established connections of one peer by direction, how many of them ride a
/// relay circuit, how many may carry data (and of those, how many we dialed),
/// how many were demoted from data, and whether it has spoken this protocol
/// version on our chain over any.
#[derive(Default)]
pub(crate) struct Links {
    pub(crate) inbound: u32,
    pub(crate) outbound: u32,
    pub(crate) relayed: u32,
    pub(crate) data: u32,
    pub(crate) outbound_data: u32,
    pub(crate) demoted: u32,
    pub(crate) on_chain: bool,
}

impl Links {
    /// Direct iff a data connection is open, relayed iff every connection
    /// rides a circuit; auxiliary or holder connections alone give no link.
    pub(crate) fn link(&self) -> Option<PeerLink> {
        match (self.data > 0, self.relayed == self.inbound + self.outbound) {
            (true, _) => Some(PeerLink::Direct),
            (false, true) => Some(PeerLink::Relayed),
            (false, false) => None,
        }
    }

    pub(crate) fn count(&mut self, outbound: bool, class: Class, opened: bool) {
        let direction = match outbound {
            true => &mut self.outbound,
            false => &mut self.inbound,
        };
        step(direction, opened);
        if class == Class::Relayed {
            step(&mut self.relayed, opened);
        }
        if class == Class::Data {
            step(&mut self.data, opened);
        }
        if class == Class::Data && outbound {
            step(&mut self.outbound_data, opened);
        }
        if class == Class::Demoted {
            step(&mut self.demoted, opened);
        }
    }
}

fn step(counter: &mut u32, opened: bool) {
    match opened {
        true => *counter += 1,
        false if *counter > 0 => *counter -= 1,
        false => (),
    }
}

#[cfg(test)]
mod tests {
    use crate::{peer_gossip::Shared, peer_limits::Class, peer_nat::PeerLink};
    use libp2p::{Multiaddr, PeerId, identity::Keypair};

    fn peer() -> PeerId {
        Keypair::generate_ed25519().public().to_peer_id()
    }

    #[test]
    fn a_circuit_remote_is_relayed_and_any_direct_connection_makes_the_peer_direct() {
        let relay = peer();
        let circuit: Multiaddr = format!("/ip4/1.2.3.4/tcp/1/p2p/{relay}/p2p-circuit")
            .parse()
            .unwrap();
        assert_eq!(PeerLink::of(&circuit), PeerLink::Relayed);
        assert_eq!(
            PeerLink::of(&"/ip4/1.2.3.4/udp/1/quic-v1".parse().unwrap()),
            PeerLink::Direct
        );
        let shared = Shared::new(peer());
        let remote = peer();
        shared.opened(remote, false, Class::Relayed);
        assert!(!shared.has_direct(remote));
        assert_eq!(shared.links(), vec![(remote, PeerLink::Relayed)]);
        shared.opened(remote, false, Class::Auxiliary);
        assert!(
            !shared.has_direct(remote),
            "only a data connection carries data"
        );
        assert!(
            shared.links().is_empty(),
            "an auxiliary connection is no link"
        );
        shared.closed(remote, false, Class::Auxiliary);
        shared.closed(remote, false, Class::Relayed);
        shared.opened(remote, false, Class::Holder);
        assert!(shared.links().is_empty(), "nor is a reservation holder");
        shared.closed(remote, false, Class::Holder);
        shared.opened(remote, false, Class::Relayed);
        shared.opened(remote, true, Class::Data);
        assert!(shared.has_direct(remote));
        shared.closed(remote, false, Class::Relayed);
        assert_eq!(shared.links(), vec![(remote, PeerLink::Direct)]);
        shared.closed(remote, true, Class::Data);
        assert!(shared.links().is_empty());
    }

    #[test]
    fn a_demoted_peer_answers_gossip_but_is_neither_direct_nor_live() {
        let shared = Shared::new(peer());
        let remote = peer();
        shared.opened(remote, true, Class::Data);
        shared.on_chain(remote);
        assert_eq!(shared.live(), vec![remote]);
        shared.reclassed(remote, true, (Class::Data, Class::Demoted));
        assert!(shared.can_gossip(remote));
        assert!(!shared.has_direct(remote));
        assert!(shared.live().is_empty());
        assert!(shared.links().is_empty(), "a demoted connection is no link");
        assert!(shared.is_connected(remote));
        shared.reclassed(remote, true, (Class::Demoted, Class::Data));
        assert_eq!(shared.live(), vec![remote]);
        shared.reclassed(remote, true, (Class::Data, Class::Demoted));
        shared.closed(remote, true, Class::Demoted);
        assert!(!shared.can_gossip(remote));
        assert!(!shared.is_connected(remote));
    }

    #[test]
    fn a_relayed_only_peer_is_neither_live_nor_outbound_until_it_has_a_direct_link() {
        let shared = Shared::new(peer());
        let remote = peer();
        shared.opened(remote, true, Class::Relayed);
        shared.on_chain(remote);
        assert!(shared.is_connected(remote));
        assert!(shared.live().is_empty());
        assert!(!shared.is_outbound(remote));
        shared.opened(remote, true, Class::Auxiliary);
        shared.opened(remote, false, Class::Data);
        assert_eq!(shared.live(), vec![remote]);
        assert!(
            !shared.is_outbound(remote),
            "our dial is auxiliary, the data connection is theirs"
        );
        shared.opened(remote, true, Class::Data);
        assert_eq!(shared.live(), vec![remote]);
        assert!(shared.is_outbound(remote));
    }
}
