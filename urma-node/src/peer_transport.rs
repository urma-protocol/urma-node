//! The URMA overlay swarm: TCP and QUIC on the same port, Noise, bounded
//! yamux, the relay client transport and, when the system resolver
//! configuration can be read, DNS. Without it the overlay still runs for
//! IP-literal peers; named addresses (`/dns*`, built-in `/dnsaddr` seeds) are
//! then skipped and the reason is reported, never a public resolver.
//!
//! NAT traversal: AutoNAT v2 client and server, a circuit relay v2 server
//! that admits reservations and circuits only while this node is public, the
//! relay client, and DCUtR hole punching.
use crate::{
    Error,
    peer::REQUEST_TIMEOUT,
    peer_codec::Codec,
    peer_fence::{Candidates, Fenced},
    peer_limits::{Classes, Limits},
    peer_protocol::PROTOCOL,
    peer_rewaken::Rewaken,
};
use libp2p::{
    Multiaddr, PeerId, StreamProtocol, Swarm, SwarmBuilder, autonat, dcutr, identify,
    identity::Keypair, multiaddr::Protocol, noise, quic, relay, request_response,
    swarm::NetworkBehaviour, tcp, yamux,
};
use std::{
    num::{NonZeroU8, NonZeroUsize},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(NetworkBehaviour)]
pub(crate) struct Behaviour {
    // Admission must precede every other behaviour's connection side effects:
    // it classifies the connection the fenced behaviours read.
    pub(crate) limits: Limits,
    pub(crate) records: Fenced<request_response::Behaviour<Codec>>,
    pub(crate) identify: Fenced<identify::Behaviour>,
    pub(crate) candidates: Candidates,
    pub(crate) autonat: autonat::v2::client::Behaviour,
    pub(crate) autonat_server: autonat::v2::server::Behaviour,
    pub(crate) relay: Rewaken<relay::Behaviour>,
    pub(crate) relay_client: relay::client::Behaviour,
    pub(crate) dcutr: dcutr::Behaviour,
}

impl Behaviour {
    const IDENTIFY_PROTOCOL: &str = "/urma/node/4";
    const STREAMS: u32 = 8;
    const WINDOW: u32 = 256 * 1024;
    // Fly's UDP path carries about 1300 bytes; quinn's default probe ceiling
    // (1452) loses packets there.
    const QUIC_MTU: u16 = 1280;
}

/// What the network loop and the behaviours share: connection classes and
/// whether this node's relay server admits reservations right now.
#[derive(Clone)]
pub(crate) struct Wiring {
    pub(crate) classes: Classes,
    pub(crate) relay_open: Arc<AtomicBool>,
}

/// Admits reservations and circuits only while the relay is open (this node
/// is public), and never for this node itself.
struct RelayGate {
    open: Arc<AtomicBool>,
    local: PeerId,
}

impl relay::RateLimiter for RelayGate {
    fn try_next(&mut self, peer: PeerId, _: &Multiaddr, _: Instant) -> bool {
        self.open.load(Ordering::Acquire) && peer != self.local
    }
}

/// §3.3 limits. libp2p denies a new reservation or circuit only when the
/// peer already holds MORE than the per-peer maximum, so 0 admits one
/// reservation per peer and 3 admits four circuits.
fn relay_config(open: &Arc<AtomicBool>, local: PeerId) -> relay::Config {
    let gate = || RelayGate {
        open: Arc::clone(open),
        local,
    };
    let mut config = relay::Config {
        max_reservations: 128,
        max_reservations_per_peer: 0,
        max_circuits: 16,
        max_circuits_per_peer: 3,
        max_circuit_duration: Duration::from_secs(2 * 60),
        max_circuit_bytes: 1 << 20,
        ..relay::Config::default()
    };
    config.reservation_rate_limiters.push(Box::new(gate()));
    config.circuit_src_rate_limiters.push(Box::new(gate()));
    config
}

/// How the overlay resolves names. `Refuse` stands in for an unreadable
/// system configuration (tests inject it); it is never a fallback resolver.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DnsMode {
    System,
    Refuse(String),
}

/// Whether this swarm can resolve names, and why not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Dns {
    Available,
    Unavailable(String),
}

impl Dns {
    /// Names need the resolver; IP literals never do.
    pub fn admits(&self, address: &Multiaddr) -> Result<(), Error> {
        let named = address.iter().any(|part| {
            matches!(
                part,
                Protocol::Dns(_) | Protocol::Dns4(_) | Protocol::Dns6(_) | Protocol::Dnsaddr(_)
            )
        });
        match (self, named) {
            (Self::Unavailable(cause), true) => Err(Error::Transport(format!(
                "system DNS resolver unavailable: {cause}"
            ))),
            (Self::Unavailable(_) | Self::Available, false) | (Self::Available, true) => Ok(()),
        }
    }
}

fn behaviour(
    identity: &Keypair,
    relay_client: relay::client::Behaviour,
    wiring: &Wiring,
) -> Behaviour {
    let local = identity.public().to_peer_id();
    let records = request_response::Behaviour::with_codec(
        Codec,
        [(
            StreamProtocol::new(PROTOCOL),
            request_response::ProtocolSupport::Full,
        )],
        request_response::Config::default()
            .with_request_timeout(REQUEST_TIMEOUT)
            .with_max_concurrent_streams(2),
    );
    let identify = identify::Behaviour::new(
        identify::Config::new(Behaviour::IDENTIFY_PROTOCOL.into(), identity.public())
            .with_agent_version(format!("urma-node/{}", env!("CARGO_PKG_VERSION")))
            .with_push_listen_addr_updates(true),
    );
    Behaviour {
        limits: Limits::new(wiring.classes.clone(), Arc::clone(&wiring.relay_open)),
        records: Fenced::data_only(records, wiring.classes.clone()),
        identify: Fenced::without_candidates(identify),
        candidates: Candidates::default(),
        autonat: autonat::v2::client::Behaviour::default(),
        autonat_server: autonat::v2::server::Behaviour::default(),
        relay: Rewaken(relay::Behaviour::new(
            local,
            relay_config(&wiring.relay_open, local),
        )),
        relay_client,
        dcutr: dcutr::Behaviour::new(local),
    }
}

fn multiplexer() -> yamux::Config {
    let mut config = yamux::Config::default();
    // These setters select libp2p-yamux's bounded 0.12 backend.
    config.set_max_num_streams(8);
    config.set_max_buffer_size(256 * 1024);
    config.set_receive_window_size(Behaviour::WINDOW);
    config
}

/// QUIC with the same stream and window bounds as yamux.
fn quic(mut config: quic::Config) -> quic::Config {
    config.max_concurrent_stream_limit = Behaviour::STREAMS;
    config.max_stream_data = Behaviour::WINDOW;
    config.max_connection_data = Behaviour::WINDOW * Behaviour::STREAMS;
    config.mtu_upper_bound(Behaviour::QUIC_MTU)
}

fn configure(config: libp2p::swarm::Config) -> libp2p::swarm::Config {
    config
        .with_notify_handler_buffer_size(NonZeroUsize::new(2).unwrap())
        .with_per_connection_event_buffer_size(2)
        .with_max_negotiating_inbound_streams(2)
        .with_dial_concurrency_factor(NonZeroU8::MIN)
        .with_idle_connection_timeout(Duration::from_secs(20))
}

fn transport_error(error: impl std::fmt::Display) -> Error {
    Error::Invalid(format!("peer transport: {error}"))
}

fn behaviour_error(error: impl std::fmt::Display) -> Error {
    Error::Invalid(format!("peer behaviour: {error}"))
}

/// Tries the system resolver first; if its configuration cannot be read the
/// same swarm is built without DNS and the cause is returned.
pub(crate) fn swarm(
    identity: Keypair,
    mode: &DnsMode,
    wiring: &Wiring,
) -> Result<(Swarm<Behaviour>, Dns), Error> {
    let transports = || {
        SwarmBuilder::with_existing_identity(identity.clone())
            .with_tokio()
            .with_tcp(
                tcp::Config::default().nodelay(true),
                noise::Config::new,
                multiplexer,
            )
            .map(|builder| builder.with_quic_config(quic))
            .map_err(transport_error)
    };
    let cause = match mode {
        DnsMode::System => match transports()?.with_dns() {
            Ok(resolving) => {
                let swarm = resolving
                    .with_relay_client(noise::Config::new, multiplexer)
                    .map_err(transport_error)?
                    .with_behaviour(|key, relay| behaviour(key, relay, wiring))
                    .map_err(behaviour_error)?
                    .with_swarm_config(configure)
                    .with_connection_timeout(Duration::from_secs(5))
                    .build();
                return Ok((swarm, Dns::Available));
            }
            Err(error) => error.to_string(),
        },
        DnsMode::Refuse(cause) => cause.clone(),
    };
    tracing::warn!(%cause, "system DNS resolver unavailable; overlay runs for IP-literal peers only");
    let swarm = transports()?
        .with_relay_client(noise::Config::new, multiplexer)
        .map_err(transport_error)?
        .with_behaviour(|key, relay| behaviour(key, relay, wiring))
        .map_err(behaviour_error)?
        .with_swarm_config(configure)
        .with_connection_timeout(Duration::from_secs(5))
        .build();
    Ok((swarm, Dns::Unavailable(cause)))
}
