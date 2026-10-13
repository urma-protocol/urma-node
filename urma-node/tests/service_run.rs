//! `urma-node service run` serves exactly what service.json says, binds the
//! control API well inside the installer's 120 s readiness window and stops
//! cleanly. Chain peers point at a closed local port.
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpStream},
    path::Path,
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use urma_node::platform::{Paths, ServiceSettings, ensure_defaults};

const READY: Duration = Duration::from_secs(120);

struct Owned(Child);

impl Drop for Owned {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Running {
    child: Owned,
    address: SocketAddr,
    token: String,
    _root: tempfile::TempDir,
}

fn layout(root: &Path) -> Paths {
    #[cfg(windows)]
    {
        Paths::windows_under(root)
    }
    #[cfg(not(windows))]
    {
        Paths::linux_under(root)
    }
}

fn http(address: SocketAddr, line: &str, token: &str) -> String {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(10)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(
        stream,
        "{line} HTTP/1.1\r\nHost: loopback\r\nConnection: close\r\nContent-Length: 0\r\nAuthorization: Bearer {token}\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

fn start() -> Running {
    let root = tempfile::tempdir().unwrap();
    let paths = layout(root.path());
    ensure_defaults(&paths).unwrap();
    let mut settings = ServiceSettings::read(&paths.settings).unwrap();
    settings.listen = "127.0.0.1:0".parse().unwrap();
    settings.urma_listen = Some("127.0.0.1:0".parse().unwrap());
    settings.chain_peers = vec!["127.0.0.1:9".parse().unwrap()];
    settings.commit_public = false;
    std::fs::write(&paths.settings, serde_json::to_vec(&settings).unwrap()).unwrap();
    let started = Instant::now();
    let mut child = Owned(
        Command::new(env!("CARGO_BIN_EXE_urma-node"))
            .args(["service", "run", "--settings"])
            .arg(&paths.settings)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let stderr = child.0.stderr.take().unwrap();
    let (lines, received) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            if lines.send(line).is_err() {
                break;
            }
        }
    });
    let address: SocketAddr = loop {
        let left = READY.saturating_sub(started.elapsed());
        let line = received
            .recv_timeout(left)
            .expect("control API listening within the 120 s readiness window");
        if let Some(address) = line
            .strip_prefix("URMA Node listening at ")
            .and_then(|rest| rest.strip_suffix(" (authenticated loopback)"))
        {
            break address.parse().unwrap();
        }
    };
    assert!(address.ip().is_loopback());
    let token = std::fs::read_to_string(&paths.token).unwrap().trim().to_string();
    let response = http(address, "GET /v1/status", &token);
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("\"application_start\""), "{response}");
    assert!(http(address, "GET /v1/status", "wrong").starts_with("HTTP/1.1 401"));
    Running {
        child,
        address,
        token,
        _root: root,
    }
}

fn exits_cleanly(child: &mut Owned) {
    let deadline = Instant::now() + Duration::from_secs(60);
    let exit = loop {
        if let Some(exit) = child.0.try_wait().unwrap() {
            break exit;
        }
        assert!(Instant::now() < deadline, "service run did not stop");
        thread::sleep(Duration::from_millis(20));
    };
    assert!(exit.success(), "{exit:?}");
}

#[test]
fn service_run_serves_service_json_and_stops_on_authenticated_shutdown() {
    let mut running = start();
    let reply = http(running.address, "POST /v1/shutdown", &running.token);
    assert!(reply.starts_with("HTTP/1.1 2"), "{reply}");
    exits_cleanly(&mut running.child);
}

#[test]
#[cfg(unix)]
fn service_run_stops_on_sigterm() {
    unsafe extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }
    let mut running = start();
    let pid = i32::try_from(running.child.0.id()).unwrap();
    assert_eq!(unsafe { kill(pid, 15) }, 0);
    exits_cleanly(&mut running.child);
}

#[test]
fn service_run_refuses_unknown_settings_before_binding() {
    let root = tempfile::tempdir().unwrap();
    let paths = layout(root.path());
    ensure_defaults(&paths).unwrap();
    std::fs::remove_file(&paths.settings).unwrap();
    std::fs::write(&paths.settings, br#"{"surprise": true}"#).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_urma-node"))
        .args(["service", "run", "--settings"])
        .arg(&paths.settings)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unknown field"), "{stderr}");
    assert!(!stderr.contains("listening"), "{stderr}");
}
