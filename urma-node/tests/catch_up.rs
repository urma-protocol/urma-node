//! Catch-up scanning: the sync thread scans back to back while behind the
//! selected tip, within a block and time budget, and stops between blocks on
//! cancellation; maintenance runs on its own thread and cadence, so a slow
//! maintenance step neither delays the scan nor runs more often than paced.
//! Mock-selected witness-authenticated blocks; no PoW or consensus claim.
#![cfg(target_os = "linux")]
#[allow(dead_code)]
mod common;
use bitcoin::{Block, BlockHash, Txid, block::Header};
use common::{MemorySource, config, profile, witness_block};
use serde_json::Value;
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use urma_chain::observation::Chain;
use urma_node::{
    Error,
    engine::Engine,
    model::{ChainPoint, Retention},
    service::{Service, ShutdownHandle},
    source::{ChainSource, CommitSource, CommitSources},
};

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const SLOW_FETCH: Duration = Duration::from_secs(3);
const PAUSE: Duration = Duration::from_secs(2);

fn chain_of(extra: &[Block], empty: u32) -> Vec<Block> {
    let mut blocks = extra.to_vec();
    for nonce in 0..empty {
        blocks.push(witness_block(&[], blocks.last(), 100 + nonce));
    }
    blocks
}

fn open(directory: &std::path::Path, blocks: &[Block], source: Arc<dyn ChainSource>) -> Engine {
    Engine::open(
        directory,
        config(&blocks[0], Retention::Unpruned),
        Some(source),
    )
    .unwrap()
}

fn status(address: SocketAddr) -> Value {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let request = format!(
        "GET /v1/status HTTP/1.1\r\nHost: loopback\r\nConnection: close\r\nAuthorization: Bearer {TOKEN}\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).unwrap();
    let mut raw = Vec::new();
    stream.take(1024 * 1024).read_to_end(&mut raw).unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    serde_json::from_slice(&raw[split + 4..]).unwrap()
}

fn scanned(status: &Value) -> u64 {
    status["archive"]["scanned_blocks"].as_u64().unwrap()
}

struct Running {
    address: SocketAddr,
    handle: ShutdownHandle,
    worker: Option<JoinHandle<Result<(), Error>>>,
}

impl Running {
    fn start(engine: Engine, sources: Option<CommitSources>) -> Self {
        let mut service = Service::bind(
            "127.0.0.1:0".parse().unwrap(),
            TOKEN.into(),
            Arc::new(engine),
        )
        .unwrap();
        if let Some(sources) = sources {
            service = service.with_commit_sources(sources).unwrap();
        }
        let address = service.address().unwrap();
        let handle = service.shutdown_handle();
        let worker = Some(thread::spawn(move || service.run(true)));
        Self {
            address,
            handle,
            worker,
        }
    }

    /// Poll until `ready`, returning the elapsed time since `since`.
    fn until(&self, since: Instant, bound: Duration, ready: impl Fn(&Value) -> bool) -> Duration {
        loop {
            let current = status(self.address);
            if ready(&current) {
                return since.elapsed();
            }
            assert!(
                since.elapsed() < bound,
                "not reached in {bound:?}: {current}"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn stop(mut self) {
        self.handle.request();
        let worker = self.worker.take().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !worker.is_finished() {
            assert!(Instant::now() < deadline, "service join exceeded bound");
            thread::sleep(Duration::from_millis(10));
        }
        worker.join().unwrap().unwrap();
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.handle.request();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[test]
fn a_service_behind_the_tip_scans_back_to_back_without_round_pauses() {
    let blocks = chain_of(&[witness_block(&[], None, 1)], 299);
    let directory = tempfile::tempdir().unwrap();
    let source = Arc::new(MemorySource::new(&blocks));
    let engine = open(directory.path(), &blocks, source);
    let started = Instant::now();
    let running = Running::start(engine, None);
    let total = u64::try_from(blocks.len()).unwrap();
    // One block per two-second round would need ten minutes.
    let took = running.until(started, Duration::from_secs(30), |s| scanned(s) == total);
    assert!(took < Duration::from_secs(20), "{took:?}");
    let done = status(running.address);
    assert!(done["last_sync_error"].is_null(), "{done}");
    running.stop();
}

/// Counts body fetches; raises `stop` while answering the `at`-th one.
struct Stopping {
    inner: Arc<MemorySource>,
    calls: AtomicUsize,
    at: usize,
    stop: Arc<AtomicBool>,
}

impl ChainSource for Stopping {
    fn genesis(&self) -> Result<BlockHash, Error> {
        self.inner.genesis()
    }
    fn tip(&self) -> Result<ChainPoint, Error> {
        self.inner.tip()
    }
    fn header(&self, height: u64) -> Result<Header, Error> {
        self.inner.header(height)
    }
    fn block(&self, height: u64) -> Result<Vec<u8>, Error> {
        if self.calls.fetch_add(1, Ordering::AcqRel) + 1 == self.at {
            self.stop.store(true, Ordering::Release);
        }
        self.inner.block(height)
    }
}

#[test]
fn catch_up_respects_its_block_and_time_budgets_and_stops_between_blocks() {
    let blocks = chain_of(&[witness_block(&[], None, 1)], 19);
    let directory = tempfile::tempdir().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let source = Arc::new(Stopping {
        inner: Arc::new(MemorySource::new(&blocks)),
        calls: AtomicUsize::new(0),
        at: 9,
        stop: Arc::clone(&stop),
    });
    let engine = open(directory.path(), &blocks, source.clone());
    engine.synchronize_headers().unwrap();
    let idle = AtomicBool::new(false);
    let pass = engine.scan_catch_up(&idle, 5, Duration::from_secs(60));
    assert_eq!(pass.scanned, 5);
    assert_eq!(pass.outcome.unwrap().scanned_blocks, 5);
    let none = engine.scan_catch_up(&idle, 5, Duration::ZERO);
    assert_eq!(none.scanned, 0);
    assert_eq!(none.outcome.unwrap().scanned_blocks, 5);
    // Stop raised while the ninth body is fetched: that block is not
    // committed, and no further block is requested.
    let cancelled = engine.scan_catch_up(&stop, 100, Duration::from_secs(60));
    assert_eq!(cancelled.scanned, 3);
    assert!(matches!(cancelled.outcome, Err(Error::Busy(_))));
    assert_eq!(source.calls.load(Ordering::Acquire), 9);
    assert_eq!(engine.status().unwrap().scanned_blocks, 8);
    // The rest of the chain reaches the tip within one pass and then idles.
    let rest = engine.scan_catch_up(&idle, 100, Duration::from_secs(60));
    assert_eq!(rest.scanned, 12);
    let tip = engine.scan_catch_up(&idle, 100, Duration::from_secs(60));
    assert_eq!((tip.scanned, tip.outcome.unwrap().scanned_blocks), (0, 20));
}

/// A parent source whose every fetch takes SLOW_FETCH and then fails.
#[derive(Clone)]
struct Slow {
    calls: Arc<Mutex<Vec<Instant>>>,
}

impl CommitSource for Slow {
    fn chain(&self) -> Chain {
        Chain::BitcoinRegtest
    }
    fn fetch(&self, _txid: Txid, cancelled: &AtomicBool) -> Result<Option<Vec<u8>>, Error> {
        self.calls.lock().unwrap().push(Instant::now());
        let until = Instant::now() + SLOW_FETCH;
        while Instant::now() < until && !cancelled.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(20));
        }
        Err(Error::Busy("scripted slow parent source".into()))
    }
}

#[test]
fn a_slow_maintenance_step_neither_delays_the_scan_nor_loses_its_cadence() {
    // The first block's reveal has no commit anywhere, so every maintenance
    // round asks the slow parent source once.
    let pair = profile();
    let blocks = chain_of(
        &[witness_block(std::slice::from_ref(&pair.reveal), None, 1)],
        59,
    );
    let directory = tempfile::tempdir().unwrap();
    let engine = open(
        directory.path(),
        &blocks,
        Arc::new(MemorySource::new(&blocks)),
    );
    let slow = Slow {
        calls: Arc::new(Mutex::new(Vec::new())),
    };
    let mut sources = CommitSources::new(Chain::BitcoinRegtest);
    sources.push("slow".into(), Box::new(slow.clone()));
    let started = Instant::now();
    let running = Running::start(engine, Some(sources));
    let total = u64::try_from(blocks.len()).unwrap();
    let took = running.until(started, Duration::from_secs(30), |s| scanned(s) == total);
    assert!(took < SLOW_FETCH, "scan waited on maintenance: {took:?}");
    let calls = || slow.calls.lock().unwrap().clone();
    let deadline = Instant::now() + Duration::from_secs(30);
    while calls().len() < 3 {
        assert!(
            Instant::now() < deadline,
            "maintenance stalled: {:?}",
            calls()
        );
        thread::sleep(Duration::from_millis(50));
    }
    let seen = calls();
    for pair in seen.windows(2) {
        let gap = pair[1] - pair[0];
        // Each round spends SLOW_FETCH in the source, then pauses.
        assert!(
            gap >= SLOW_FETCH + PAUSE - Duration::from_millis(300),
            "{gap:?}"
        );
        assert!(gap < SLOW_FETCH + PAUSE + Duration::from_secs(3), "{gap:?}");
    }
    let done = status(running.address);
    assert_eq!(done["archive"]["unresolved_dependencies"], 1, "{done}");
    running.stop();
}
