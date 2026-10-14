use clap::{Parser, Subcommand};
use std::{
    env, io,
    net::SocketAddr,
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};
use urma_node::{
    daemon::{self, OverlayOptions, ServeOptions, ServiceCommand},
    engine::Engine,
    install,
    platform::{CommitRpc, create_peer_key},
    service::{ShutdownHandle, create_token},
};

/// `i64::MAX` as an unsigned CLI bound: byte counts are stored as SQLite INTEGER.
const MAX_ARG: u64 = 9_223_372_036_854_775_807;

#[derive(Parser)]
#[command(about = "Optional URMA archive service")]
struct Arguments {
    /// Archive directory (status, serve).
    #[arg(long)]
    data: Option<PathBuf>,
    /// Archive config (status, serve).
    #[arg(long)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Open local state without any network operation.
    Status,
    /// Create a new private service token (no wallet keys involved).
    CreateToken { path: PathBuf },
    /// Create a separate transport identity; never a wallet or decryption key.
    CreatePeerKey { path: PathBuf },
    /// Run the authenticated loopback service in the foreground.
    Serve(Box<ServeArguments>),
    /// The installed system service (URMA_NODE_INSTALL.md).
    Service {
        #[command(subcommand)]
        action: ServiceCommand,
    },
}

#[derive(clap::Args)]
struct ServeArguments {
    #[arg(long, default_value = "127.0.0.1:28331")]
    listen: SocketAddr,
    #[arg(long)]
    token_file: PathBuf,
    /// Opt into the checkpoint-relative Litecoin source; offline by default.
    #[arg(long)]
    p2p: bool,
    /// Explicit trusted anchor and authenticated preceding header context.
    #[arg(long, requires = "p2p")]
    header_bootstrap: Option<PathBuf>,
    /// Litecoin peer to sync headers/blocks from (repeatable); replaces DNS seeds.
    #[arg(long = "chain-peer", requires = "p2p", value_name = "ADDR")]
    chain_peer: Vec<SocketAddr>,
    /// Loopback cookie RPC for missing commit parents; no default endpoint.
    #[arg(long, requires_all = ["p2p", "commit_rpc_cookie"])]
    commit_rpc_url: Option<String>,
    /// RPC cookie file authenticating --commit-rpc-url; no other RPC auth.
    #[arg(long, requires_all = ["p2p", "commit_rpc_url"])]
    commit_rpc_cookie: Option<PathBuf>,
    /// Never send missing commit TXIDs to the chain's default public providers.
    #[arg(long, requires = "p2p")]
    no_commit_public: bool,
    /// Opt into the URMA overlay using an explicitly created transport key.
    #[arg(long)]
    peer_key: Option<PathBuf>,
    /// Optional URMA peer listener, separate from the local control API: TCP and
    /// QUIC (UDP) on the same ip:port.
    #[arg(long, requires = "peer_key")]
    urma_listen: Option<SocketAddr>,
    /// Explicit IP/TCP/p2p routing hint; saved in this archive's peer book.
    #[arg(long, requires = "peer_key")]
    urma_peer: Vec<libp2p::Multiaddr>,
    /// Extra bootstrap seed (repeatable); saved with seed origin.
    #[arg(long, requires = "peer_key")]
    urma_seed: Vec<libp2p::Multiaddr>,
    /// Do not add this chain's built-in DNS seeds.
    #[arg(long, requires = "peer_key")]
    no_seeds: bool,
    /// Public IP/TCP or IP/UDP/QUIC address peers should dial (repeatable);
    /// advertised through identify.
    #[arg(long, requires = "peer_key")]
    urma_announce: Vec<libp2p::Multiaddr>,
    /// Soft reclaimable cache target (not a SQLite/WAL or total archive quota).
    #[arg(long, default_value_t = urma_node::cache::DEFAULT_CACHE_BYTES, value_parser = clap::value_parser!(u64).range(..=MAX_ARG))]
    cache_bytes: u64,
    /// Network time slice per background object round; a started fetch may finish past the
    /// slice, and an unfinished object resumes next round.
    #[arg(long, default_value_t = 15, value_parser = clap::value_parser!(u64).range(1..))]
    object_timeout_seconds: u64,
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("urma-node: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode, Box<dyn std::error::Error>> {
    let args = Arguments::parse();
    match args.command {
        Command::CreateToken { path } => create_token(&path)?,
        Command::CreatePeerKey { path } => create_peer_key(&path)?,
        Command::Status => status(
            &required(args.data, "--data")?,
            &required(args.config, "--config")?,
        )?,
        Command::Serve(serve) => {
            let data = required(args.data, "--data")?;
            let config = required(args.config, "--config")?;
            #[cfg(not(windows))]
            log_to_stderr()?;
            daemon::serve(
                serve_options(data, config, *serve),
                signals,
                &mut io::stderr(),
            )?;
        }
        Command::Service {
            action: ServiceCommand::Run { settings },
        } => {
            #[cfg(not(windows))]
            log_to_stderr()?;
            daemon::run_service(&settings, signals, &mut io::stderr())?;
        }
        Command::Service {
            action: ServiceCommand::Manage(action),
        } => return manage(action),
    }
    Ok(ExitCode::SUCCESS)
}

fn required(value: Option<PathBuf>, flag: &str) -> Result<PathBuf, String> {
    value.ok_or(format!("{flag} is required"))
}

fn status(data: &Path, config: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let engine = Engine::open(data, daemon::read_archive_config(config)?, None)?;
    println!("{}", serde_json::to_string_pretty(&engine.status()?)?);
    Ok(())
}

#[cfg(not(windows))]
fn log_to_stderr() -> Result<(), urma_node::Error> {
    let Some(directives) = env::var_os("RUST_LOG") else {
        return Ok(());
    };
    let directives = directives
        .to_str()
        .ok_or(urma_node::Error::Invalid(format!(
            "RUST_LOG is not UTF-8: {directives:?}"
        )))?;
    let filter = tracing_subscriber::EnvFilter::try_new(directives)
        .map_err(|error| urma_node::Error::Invalid(format!("RUST_LOG: {error}")))?;
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(io::stderr)
        .try_init()
        .map_err(|error| urma_node::Error::Invalid(error.to_string()))
}

/// Only the binary owns process signals: SIGINT, SIGTERM and SIGHUP (and the
/// console events on Windows) request the same cooperative stop as
/// authenticated HTTP shutdown. Inherited dispositions are replaced.
fn signals(shutdown: ShutdownHandle) -> Result<(), urma_node::Error> {
    ctrlc::set_handler(move || shutdown.request())
        .map_err(|error| urma_node::Error::Invalid(error.to_string()))
}

fn serve_options(data: PathBuf, config: PathBuf, serve: ServeArguments) -> ServeOptions {
    let commit_rpc = match (serve.commit_rpc_url, serve.commit_rpc_cookie) {
        (Some(url), Some(cookie)) => Some(CommitRpc { url, cookie }),
        (Some(_), None) | (None, Some(_)) | (None, None) => None,
    };
    ServeOptions {
        data,
        config,
        listen: serve.listen,
        token_file: serve.token_file,
        p2p: serve.p2p,
        header_bootstrap: serve.header_bootstrap,
        chain_peers: serve.chain_peer,
        commit_rpc,
        commit_public: !serve.no_commit_public,
        overlay: OverlayOptions {
            key: serve.peer_key,
            listen: serve.urma_listen,
            manual: serve.urma_peer,
            seeds: serve.urma_seed,
            builtin_seeds: !serve.no_seeds,
            announce: serve.urma_announce,
        },
        cache_bytes: serve.cache_bytes,
        object_timeout: Duration::from_secs(serve.object_timeout_seconds),
    }
}

/// install/update/uninstall/status (U2's `install::run`); no init system
/// found exits with code 3.
fn manage(action: install::Action) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let host = install::Host {
        sudo_user: env::var("SUDO_USER").unwrap_or_default(),
        sudo_uid: env::var("SUDO_UID").unwrap_or_default(),
        pkexec_uid: env::var("PKEXEC_UID").unwrap_or_default(),
        binary: env::current_exe()?,
    };
    match install::run(action, &host, &mut io::stdout().lock()) {
        Ok(()) => Ok(ExitCode::SUCCESS),
        Err(install::Error::NoInit) => {
            eprintln!("urma-node: {}", install::Error::NoInit);
            Ok(ExitCode::from(3))
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    const BASE: [&str; 8] = [
        "urma-node",
        "--data",
        "data",
        "--config",
        "config",
        "serve",
        "--token-file",
        "token",
    ];

    fn serve(args: &[&str]) -> Result<ServeArguments, clap::Error> {
        match Arguments::try_parse_from(args)?.command {
            Command::Serve(serve) => Ok(*serve),
            _ => panic!("not serve"),
        }
    }

    #[test]
    fn explicit_bootstrap_requires_p2p_and_legacy_serve_flags_remain_optional() {
        let offline = serve(&BASE).unwrap();
        assert!(!offline.p2p && offline.header_bootstrap.is_none());
        let mut explicit = BASE.to_vec();
        explicit.extend(["--header-bootstrap", "bootstrap.json"]);
        assert!(serve(&explicit).is_err());
        explicit.push("--p2p");
        let strict = serve(&explicit).unwrap();
        assert!(strict.p2p && strict.header_bootstrap.is_some());
        let mut legacy = BASE.to_vec();
        legacy.push("--p2p");
        let legacy = serve(&legacy).unwrap();
        assert!(legacy.p2p && legacy.header_bootstrap.is_none());
        let (local, fly) = ("127.0.0.1:19335", "137.66.3.26:19335");
        let mut pinned = BASE.to_vec();
        pinned.extend(["--chain-peer", local, "--chain-peer", fly]);
        assert!(serve(&pinned).is_err());
        pinned.push("--p2p");
        let peers: Vec<SocketAddr> = vec![local.parse().unwrap(), fly.parse().unwrap()];
        assert_eq!(serve(&pinned).unwrap().chain_peer, peers);
    }

    #[test]
    fn overlay_flags_require_explicit_identity_and_do_not_imply_chain_sync() {
        let base = serve(&BASE).unwrap();
        assert!(base.peer_key.is_none() && base.urma_listen.is_none() && !base.p2p);
        let mut listener = BASE.to_vec();
        listener.extend(["--urma-listen", "127.0.0.1:0"]);
        assert!(serve(&listener).is_err());
        listener.extend(["--peer-key", "transport.key"]);
        let listener = serve(&listener).unwrap();
        assert!(listener.peer_key.is_some() && listener.urma_listen.is_some() && !listener.p2p);
        let peer = libp2p::identity::Keypair::generate_ed25519()
            .public()
            .to_peer_id();
        let address = format!("/ip4/127.0.0.1/tcp/3210/p2p/{peer}");
        let mut remote = BASE.to_vec();
        remote.extend(["--urma-peer", &address]);
        assert!(serve(&remote).is_err());
        remote.extend(["--peer-key", "transport.key"]);
        assert!(serve(&remote).is_ok());
        assert_eq!(i64::try_from(MAX_ARG).unwrap(), i64::MAX);
        for (value, expected) in [("0", 0), ("9223372036854775807", MAX_ARG)] {
            let mut cache = BASE.to_vec();
            cache.extend(["--cache-bytes", value]);
            let cache = serve(&cache).unwrap();
            assert!(cache.cache_bytes == expected && !cache.p2p);
        }
        for value in ["-1", "9223372036854775808", "not-a-number"] {
            let mut cache = BASE.to_vec();
            cache.extend(["--cache-bytes", value]);
            assert!(serve(&cache).is_err());
        }
        for (option, value) in [
            ("--max-object-bytes", "1"),
            ("--max-object-nodes", "1"),
            ("--object-timeout-seconds", "0"),
        ] {
            let mut invalid = BASE.to_vec();
            invalid.extend([option, value, "--peer-key", "transport.key"]);
            assert!(serve(&invalid).is_err());
        }
    }

    #[test]
    fn service_run_reads_the_system_settings_by_default() {
        let parsed = Arguments::try_parse_from(["urma-node", "service", "run"]).unwrap();
        assert!(matches!(
            parsed.command,
            Command::Service { action: ServiceCommand::Run { settings } }
                if settings == urma_node::platform::Paths::system().settings
        ));
        for action in ["install", "update", "uninstall", "status"] {
            let parsed = Arguments::try_parse_from(["urma-node", "service", action]).unwrap();
            assert!(matches!(
                parsed.command,
                Command::Service {
                    action: ServiceCommand::Manage(_)
                }
            ));
        }
    }
}
