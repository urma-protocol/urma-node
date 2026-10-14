//! Bounds of the local-rpc commit source against misbehaving local Cores:
//! one that answers but keeps the socket open, one that answers slowly, and
//! a cookie path that is not a regular file. Owned loopback stubs only.
#![cfg(target_os = "linux")]
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use urma_chain::observation::Chain;
use urma_node::source::{CommitSources, LocalRpcCommitSource};

const COOKIE: &str = "__cookie__:5c0ffee5c0ffee5c0ffee5c0ffee5c0ffee5c0ffee5c0ffee5c0ffee5";

// A synced regtest Core that waits `delay` before each reply and, when
// `linger`, keeps every connection open after replying.
struct Core {
    url: String,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Core {
    fn start(delay: Duration, linger: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let halted = stop.clone();
        let thread = thread::spawn(move || {
            let mut held = Vec::new();
            while !halted.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        thread::sleep(delay);
                        // A client that gave up (its budget ran out) closes
                        // the socket mid-reply; that is the expected outcome.
                        let _gave_up = reply(&stream);
                        if linger {
                            held.push(stream);
                        }
                    }
                    Err(_) => thread::sleep(Duration::from_millis(5)),
                }
            }
        });
        Self {
            url,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

fn reply(mut stream: &TcpStream) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte)?;
        head.push(byte[0]);
    }
    let head = String::from_utf8(head).map_err(io::Error::other)?;
    let length: usize = head
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse().ok())
        .ok_or_else(|| io::Error::other("missing content length"))?;
    let mut body = vec![0; length];
    stream.read_exact(&mut body)?;
    let request: Value = serde_json::from_slice(&body).map_err(io::Error::other)?;
    let result = match request["method"].as_str().unwrap() {
        "getblockhash" => json!(Chain::BitcoinRegtest.genesis().unwrap().0.to_string()),
        "getindexinfo" => json!({"txindex": {"synced": true}}),
        _ => json!(1),
    };
    let reply = serde_json::to_vec(&json!({"result": result, "error": null, "id": request["id"]}))?;
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        reply.len()
    )?;
    stream.write_all(&reply)?;
    stream.flush()
}

fn sources(url: &str, cookie: &Path, budget: Duration) -> CommitSources {
    let local = LocalRpcCommitSource::new(Chain::BitcoinRegtest, url.into(), cookie.into())
        .with_probe_interval(Duration::ZERO)
        .with_budgets(budget, budget);
    let mut sources = CommitSources::new(Chain::BitcoinRegtest);
    sources.push("local-rpc".into(), Box::new(local));
    sources
}

fn probed(sources: &CommitSources) -> (Option<String>, Duration) {
    let started = Instant::now();
    sources.probe().unwrap();
    let elapsed = started.elapsed();
    (sources.status().unwrap()[0].last_error.clone(), elapsed)
}

#[test]
fn a_core_that_keeps_the_socket_open_costs_only_its_reply() {
    let core = Core::start(Duration::ZERO, true);
    let directory = tempfile::tempdir().unwrap();
    let cookie = directory.path().join("cookie");
    fs::write(&cookie, COOKIE).unwrap();
    let sources = sources(&core.url, &cookie, Duration::from_secs(3));
    let (error, elapsed) = probed(&sources);
    assert_eq!(error, None);
    assert!(elapsed < Duration::from_secs(1), "probe took {elapsed:?}");
}

#[test]
fn verification_and_the_call_share_one_budget() {
    // Each answer takes 300 ms: three calls fit three 500 ms budgets, not one.
    let core = Core::start(Duration::from_millis(300), false);
    let directory = tempfile::tempdir().unwrap();
    let cookie = directory.path().join("cookie");
    fs::write(&cookie, COOKIE).unwrap();
    let budget = Duration::from_millis(500);
    let sources = sources(&core.url, &cookie, budget);
    let (error, elapsed) = probed(&sources);
    let error = error.expect("a 900 ms probe must not fit a 500 ms budget");
    assert!(error.contains("timed out within 500 ms"), "{error}");
    assert!(
        elapsed < budget + Duration::from_millis(250),
        "probe took {elapsed:?}"
    );
}

#[test]
fn a_cookie_fifo_is_refused_without_blocking() {
    let core = Core::start(Duration::ZERO, false);
    let directory = tempfile::tempdir().unwrap();
    let cookie = directory.path().join("cookie");
    let made = Command::new("mkfifo").arg(&cookie).status().unwrap();
    assert!(made.success());
    let sources = sources(&core.url, &cookie, Duration::from_secs(3));
    let (error, elapsed) = probed(&sources);
    let error = error.expect("a FIFO is not a cookie");
    assert!(error.contains("cookie file unreadable"), "{error}");
    assert!(elapsed < Duration::from_secs(1), "probe took {elapsed:?}");
}
