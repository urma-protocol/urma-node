//! Test-side NAT client for peer_nat.rs: a raw swarm with TCP, QUIC and the
//! relay client transport, identify, the records protocol and, optionally,
//! DCUtR. It dials any address (circuits included) and sends records
//! requests over whatever connections it holds to the peer.
//! Loaded with #[path]; it is not part of the shared common module.
use libp2p::{
    Multiaddr, PeerId, StreamProtocol, Swarm, SwarmBuilder, autonat, dcutr,
    futures::StreamExt,
    identify,
    identity::Keypair,
    noise, relay,
    request_response::{self, Message, OutboundRequestId, ProtocolSupport, ResponseChannel},
    swarm::{ConnectionId, NetworkBehaviour, SwarmEvent, behaviour::toggle::Toggle},
    tcp, yamux,
};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use tokio::sync::mpsc as async_mpsc;
use urma_chain::observation::Chain;
use urma_node::{
    peer_codec::Codec,
    peer_protocol::{Kind, PROTOCOL, Reply, Request, Response, VERSION},
};

const BOUND: Duration = Duration::from_secs(15);

#[derive(NetworkBehaviour)]
struct Raw {
    records: request_response::Behaviour<Codec>,
    identify: identify::Behaviour,
    relay: relay::client::Behaviour,
    dcutr: Toggle<dcutr::Behaviour>,
    autonat: autonat::v2::client::Behaviour,
}

type Reported<T> = mpsc::SyncSender<Result<T, String>>;

enum Op {
    Dial(Multiaddr, Reported<PeerId>),
    Call(PeerId, Kind, Reported<Reply>),
    Listen(Multiaddr),
}

pub struct NatPeer {
    pub id: PeerId,
    ops: async_mpsc::Sender<Op>,
    stop: Arc<AtomicBool>,
    counts: Counts,
    probes: Probes,
    worker: Option<JoinHandle<()>>,
}

fn raw_swarm(key: Keypair, punch: bool) -> Swarm<Raw> {
    SwarmBuilder::with_existing_identity(key)
        .with_tokio()
        .with_tcp(
            tcp::Config::default().nodelay(true),
            noise::Config::new,
            yamux::Config::default,
        )
        .unwrap()
        .with_quic()
        .with_relay_client(noise::Config::new, yamux::Config::default)
        .unwrap()
        .with_behaviour(|key, relay| Raw {
            records: request_response::Behaviour::with_codec(
                Codec,
                [(StreamProtocol::new(PROTOCOL), ProtocolSupport::Full)],
                request_response::Config::default().with_request_timeout(Duration::from_secs(8)),
            ),
            identify: identify::Behaviour::new(identify::Config::new(
                "/urma/test/1".into(),
                key.public(),
            )),
            relay,
            dcutr: Toggle::from(punch.then(|| dcutr::Behaviour::new(key.public().to_peer_id()))),
            autonat: autonat::v2::client::Behaviour::default(),
        })
        .unwrap()
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(60)))
        .build()
}

type Counts = Arc<Mutex<HashMap<PeerId, u32>>>;
/// AutoNAT results: the tested address and whether the server reached it.
type Probes = Arc<Mutex<Vec<(Multiaddr, bool)>>>;

#[derive(Default)]
struct Waiting {
    dials: HashMap<ConnectionId, Reported<PeerId>>,
    calls: HashMap<OutboundRequestId, Reported<Reply>>,
    counts: Counts,
    probes: Probes,
}

fn serve(swarm: &mut Swarm<Raw>, request: Request, channel: ResponseChannel<Response>) {
    let response = Response {
        version: request.version,
        chain_genesis: request.chain_genesis,
        kind: request.kind,
        result: Reply::Peers(Vec::new()),
    };
    let _ = swarm
        .behaviour_mut()
        .records
        .send_response(channel, response);
}

fn op(swarm: &mut Swarm<Raw>, waiting: &mut Waiting, op: Op) {
    match op {
        Op::Dial(address, answer) => {
            let options = libp2p::swarm::dial_opts::DialOpts::unknown_peer_id()
                .address(address)
                .build();
            let id = options.connection_id();
            match swarm.dial(options) {
                Ok(()) => {
                    waiting.dials.insert(id, answer);
                }
                Err(error) => drop(answer.try_send(Err(error.to_string()))),
            }
        }
        Op::Call(peer, kind, answer) => {
            let request = Request {
                version: VERSION,
                chain_genesis: Chain::BitcoinRegtest.genesis().unwrap().0,
                kind,
            };
            let id = swarm.behaviour_mut().records.send_request(&peer, request);
            waiting.calls.insert(id, answer);
        }
        Op::Listen(address) => {
            swarm.listen_on(address).unwrap();
        }
    }
}

fn event(swarm: &mut Swarm<Raw>, waiting: &mut Waiting, event: SwarmEvent<RawEvent>) {
    match event {
        SwarmEvent::ConnectionEstablished {
            peer_id,
            connection_id,
            num_established,
            ..
        } => {
            waiting
                .counts
                .lock()
                .unwrap()
                .insert(peer_id, num_established.get());
            if let Some(answer) = waiting.dials.remove(&connection_id) {
                let _ = answer.try_send(Ok(peer_id));
            }
        }
        SwarmEvent::ConnectionClosed {
            peer_id,
            num_established,
            ..
        } => {
            waiting
                .counts
                .lock()
                .unwrap()
                .insert(peer_id, num_established);
        }
        SwarmEvent::Behaviour(RawEvent::Records(request_response::Event::Message {
            message: Message::Request {
                request, channel, ..
            },
            ..
        })) => serve(swarm, request, channel),
        SwarmEvent::OutgoingConnectionError {
            connection_id,
            error,
            ..
        } => {
            if let Some(answer) = waiting.dials.remove(&connection_id) {
                let _ = answer.try_send(Err(error.to_string()));
            }
        }
        SwarmEvent::Behaviour(RawEvent::Records(request_response::Event::Message {
            message:
                Message::Response {
                    request_id,
                    response,
                },
            ..
        })) => answer(waiting, request_id, Ok(response)),
        SwarmEvent::Behaviour(RawEvent::Records(request_response::Event::OutboundFailure {
            request_id,
            error,
            ..
        })) => answer(waiting, request_id, Err(error.to_string())),
        SwarmEvent::Behaviour(RawEvent::Autonat(probe)) => {
            let reached = probe.result.is_ok();
            waiting
                .probes
                .lock()
                .unwrap()
                .push((probe.tested_addr, reached));
        }
        _ => (),
    }
}

fn answer(waiting: &mut Waiting, id: OutboundRequestId, result: Result<Response, String>) {
    if let Some(answer) = waiting.calls.remove(&id) {
        let _ = answer.try_send(result.map(|response| response.result));
    }
}

async fn run(
    mut swarm: Swarm<Raw>,
    mut ops: async_mpsc::Receiver<Op>,
    (stop, counts, probes): (Arc<AtomicBool>, Counts, Probes),
) {
    let mut waiting = Waiting {
        counts,
        probes,
        ..Waiting::default()
    };
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    while !stop.load(Ordering::Acquire) {
        tokio::select! {
            _ = tick.tick() => {}
            next = ops.recv() => {
                let Some(next) = next else { return };
                op(&mut swarm, &mut waiting, next);
            }
            next = swarm.select_next_some() => event(&mut swarm, &mut waiting, next),
        }
    }
}

impl NatPeer {
    /// Listens on `listen` (TCP, so dials reuse its port) when given; DCUtR
    /// runs only with `punch`.
    pub fn start(listen: Option<Multiaddr>, punch: bool) -> Self {
        let key = Keypair::generate_ed25519();
        let id = key.public().to_peer_id();
        let (ops, receiver) = async_mpsc::channel(4);
        let stop = Arc::new(AtomicBool::new(false));
        let counts = Counts::default();
        let probes = Probes::default();
        let shared = (stop.clone(), counts.clone(), probes.clone());
        let worker = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let mut swarm = raw_swarm(key, punch);
                if let Some(address) = listen {
                    swarm.listen_on(address).unwrap();
                }
                run(swarm, receiver, shared).await
            });
        });
        Self {
            id,
            ops,
            stop,
            counts,
            probes,
            worker: Some(worker),
        }
    }

    /// AutoNAT results so far: tested address and whether it was reached.
    pub fn probes(&self) -> Vec<(Multiaddr, bool)> {
        self.probes.lock().unwrap().clone()
    }

    /// Connections currently open to `peer`.
    pub fn connections(&self, peer: PeerId) -> u32 {
        self.counts.lock().unwrap().get(&peer).copied().unwrap_or(0)
    }

    pub fn dial(&self, address: Multiaddr) -> Result<PeerId, String> {
        let (answer, receiver) = mpsc::sync_channel(1);
        self.ops.try_send(Op::Dial(address, answer)).unwrap();
        receiver.recv_timeout(BOUND).unwrap()
    }

    /// Listens on `address`; a `/p2p-circuit` address reserves on its relay.
    pub fn listen(&self, address: Multiaddr) {
        self.ops.try_send(Op::Listen(address)).unwrap();
    }

    pub fn call(&self, peer: PeerId, kind: Kind) -> Result<Reply, String> {
        let (answer, receiver) = mpsc::sync_channel(1);
        self.ops.try_send(Op::Call(peer, kind, answer)).unwrap();
        receiver.recv_timeout(BOUND).unwrap()
    }
}

impl Drop for NatPeer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                eprintln!("nat peer worker panicked");
            }
        }
    }
}
