//! Address shapes the overlay accepts. A direct address is `<host>/tcp/<port>`
//! or `<host>/udp/<port>/quic-v1` (host an IP literal or DNS name), or
//! `/dnsaddr/<name>`. A circuit address reaches its peer through a relay:
//! `/ip4/<public relay IP>/<transport>/p2p/<relay>/p2p-circuit`. Either may
//! end in `/p2p/<peer>`. Shape checks never resolve, dial or trust anything.
use crate::{Error, peer_gossip::is_public};
use libp2p::{Multiaddr, PeerId, multiaddr::Protocol};
use std::net::IpAddr;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddressKind {
    Direct,
    /// Reached through this relay, which also coordinates hole punching.
    Circuit(PeerId),
}

/// What a dial reached. A relayed-only peer is not reached yet: records/5
/// (and with it the chain probe) never runs over a circuit, so the peer
/// counts once DCUtR has given it a direct connection and the probe passed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialed {
    Direct(PeerId),
    Relayed(PeerId),
}

/// A dialable address split into its bare form (no terminal `/p2p`), the
/// identity it names and its kind.
pub struct Shape {
    pub bare: Multiaddr,
    pub peer: Option<PeerId>,
    pub kind: AddressKind,
}

/// Validate one address. IPs must be usable destinations; a circuit's relay
/// hop is a public IPv4 literal and its relay is never the peer it names.
pub fn parse(address: &Multiaddr) -> Result<Shape, Error> {
    let invalid = || Error::Invalid("peer address shape is not dialable".into());
    let parts: Vec<Protocol<'_>> = address.iter().collect();
    let (peer, body) = match parts.split_last() {
        Some((Protocol::P2p(peer), body)) => (Some(*peer), body),
        _ => (None, &parts[..]),
    };
    let kind = match body {
        [Protocol::Dnsaddr(_)] => AddressKind::Direct,
        [host, transport @ ..] if direct_host(host) && transport_port(transport) => {
            AddressKind::Direct
        }
        [
            Protocol::Ip4(_),
            transport @ ..,
            Protocol::P2p(relay),
            Protocol::P2pCircuit,
        ] if transport_port(transport) && peer != Some(*relay) => AddressKind::Circuit(*relay),
        _ => return Err(invalid()),
    };
    let bare: Multiaddr = body.iter().cloned().collect();
    if matches!(kind, AddressKind::Circuit(_)) && !is_public(&bare) {
        return Err(Error::Invalid("circuit relay must be a public IPv4".into()));
    }
    Ok(Shape { bare, peer, kind })
}

/// Whether the address goes through a relay; shape is not checked.
pub fn is_circuit(address: &Multiaddr) -> bool {
    address.iter().any(|part| part == Protocol::P2pCircuit)
}

/// The relay of a valid circuit address.
pub fn relay(address: &Multiaddr) -> Option<PeerId> {
    match parse(address).ok()?.kind {
        AddressKind::Circuit(relay) => Some(relay),
        AddressKind::Direct => None,
    }
}

fn direct_host(host: &Protocol<'_>) -> bool {
    match host {
        Protocol::Ip4(ip) => usable(IpAddr::V4(*ip)),
        Protocol::Ip6(ip) => usable(IpAddr::V6(*ip)),
        Protocol::Dns(_) | Protocol::Dns4(_) | Protocol::Dns6(_) => true,
        _ => false,
    }
}

/// Exactly a nonzero TCP port, or a nonzero UDP port carrying QUIC v1.
fn transport_port(transport: &[Protocol<'_>]) -> bool {
    match transport {
        [Protocol::Tcp(port)] | [Protocol::Udp(port), Protocol::QuicV1] => *port != 0,
        _ => false,
    }
}

pub(crate) fn usable(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => !ip.is_unspecified() && !ip.is_multicast() && !ip.is_broadcast(),
        IpAddr::V6(ip) => !ip.is_unspecified() && !ip.is_multicast(),
    }
}

/// A syntactically validated sync source: a direct IP-literal address with a
/// terminal `/p2p` naming the peer. Never a circuit: data stays off relays.
/// Validation establishes neither reachability nor trust in the named peer.
#[derive(Clone, Debug)]
pub struct PeerAddress {
    peer: PeerId,
    address: Multiaddr,
}

impl PeerAddress {
    /// Loopback and private addresses are allowed; unspecified, multicast and
    /// IPv4 broadcast destinations are refused. No resolution or dial occurs.
    pub fn new(peer: PeerId, address: Multiaddr) -> Result<Self, Error> {
        let refused = || {
            Error::Invalid(
                "peer address requires a usable IP, nonzero TCP or QUIC port and terminal matching peer ID"
                    .into(),
            )
        };
        let shape = parse(&address).map_err(|_| refused())?;
        let literal = matches!(
            shape.bare.iter().next(),
            Some(Protocol::Ip4(_) | Protocol::Ip6(_))
        );
        if shape.kind != AddressKind::Direct || !literal || shape.peer != Some(peer) {
            return Err(refused());
        }
        Ok(Self { peer, address })
    }

    pub fn peer_id(&self) -> PeerId {
        self.peer
    }

    pub fn address(&self) -> &Multiaddr {
        &self.address
    }
}
