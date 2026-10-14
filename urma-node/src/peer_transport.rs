//! The URMA overlay swarm: TCP, Noise, bounded yamux and, when the system
//! resolver configuration can be read, DNS. Without it the overlay still runs
//! for IP-literal peers; named addresses (`/dns*`, built-in `/dnsaddr` seeds)
//! are then skipped and the reason is reported, never a public resolver.
use crate::{
    Error, peer::REQUEST_TIMEOUT, peer_codec::Codec, peer_limits::Limits, peer_protocol::PROTOCOL,
};
use libp2p::{
    Multiaddr, StreamProtocol, Swarm, SwarmBuilder, identify, identity::Keypair,
    multiaddr::Protocol, noise, request_response, swarm::NetworkBehaviour, tcp, yamux,
};
use std::{
    num::{NonZeroU8, NonZeroUsize},
    time::Duration,
};

#[derive(NetworkBehaviour)]
pub(crate) struct Behaviour {
    // Admission must precede request-response connection side effects.
    pub(crate) limits: Limits,
    pub(crate) records: request_response::Behaviour<Codec>,
    pub(crate) identify: identify::Behaviour,
}

impl Behaviour {
    const IDENTIFY_PROTOCOL: &str = "/urma/node/4";
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

fn behaviour(identity: &Keypair) -> Behaviour {
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
            .with_agent_version(format!("urma-node/{}", env!("CARGO_PKG_VERSION"))),
    );
    Behaviour {
        limits: Limits::default(),
        records,
        identify,
    }
}

fn multiplexer() -> yamux::Config {
    let mut config = yamux::Config::default();
    // These setters select libp2p-yamux's bounded 0.12 backend.
    config.set_max_num_streams(4);
    config.set_max_buffer_size(256 * 1024);
    config.set_receive_window_size(256 * 1024);
    config
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
pub(crate) fn swarm(identity: Keypair, mode: &DnsMode) -> Result<(Swarm<Behaviour>, Dns), Error> {
    let cause = match mode {
        DnsMode::System => {
            let tcp = SwarmBuilder::with_existing_identity(identity.clone())
                .with_tokio()
                .with_tcp(
                    tcp::Config::default().nodelay(true),
                    noise::Config::new,
                    multiplexer,
                )
                .map_err(transport_error)?;
            match tcp.with_dns() {
                Ok(resolving) => {
                    let swarm = resolving
                        .with_behaviour(behaviour)
                        .map_err(behaviour_error)?
                        .with_swarm_config(configure)
                        .with_connection_timeout(Duration::from_secs(5))
                        .build();
                    return Ok((swarm, Dns::Available));
                }
                Err(error) => error.to_string(),
            }
        }
        DnsMode::Refuse(cause) => cause.clone(),
    };
    tracing::warn!(%cause, "system DNS resolver unavailable; overlay runs for IP-literal peers only");
    let swarm = SwarmBuilder::with_existing_identity(identity)
        .with_tokio()
        .with_tcp(
            tcp::Config::default().nodelay(true),
            noise::Config::new,
            multiplexer,
        )
        .map_err(transport_error)?
        .with_behaviour(behaviour)
        .map_err(behaviour_error)?
        .with_swarm_config(configure)
        .with_connection_timeout(Duration::from_secs(5))
        .build();
    Ok((swarm, Dns::Unavailable(cause)))
}
