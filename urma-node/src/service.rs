//! Small authenticated loopback control surface. The archive engine remains
//! usable directly; HTTP is not a prerequisite for embedded applications.
use crate::{
    Error,
    engine::Engine,
    model::{CacheReceipt, ServiceStatus},
    peer::SyncPeers,
    peer_discovery::Discovery,
    peer_sync::Scheduler,
    recorded::{RecordedError, read},
    retrieval::RecordSource,
    source::{CommitSource, CommitSources},
};

mod overlay;
mod worker;
use rand::RngCore;
use serde_json::json;
use std::{
    fs::File,
    io::{Read, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use urma_io::Access;

#[derive(Default, Clone)]
struct CacheStatus {
    receipt: Option<CacheReceipt>,
    error: Option<RecordedError>,
}

type ErrorSlot = Arc<Mutex<Option<RecordedError>>>;

/// Cooperative stop request shared with the owner of a running service.
/// Requesting shutdown does not wait for completion or interrupt entered I/O
/// or SQL. The owner must wait for `Service::run` to return before reopening
/// its archive. Embedded services do not install process signal handlers.
#[derive(Clone)]
pub struct ShutdownHandle {
    stop: Arc<AtomicBool>,
}

impl ShutdownHandle {
    pub fn request(&self) {
        self.stop.store(true, Ordering::Release);
    }
}

pub struct Service {
    listener: TcpListener,
    token: String,
    engine: Arc<Engine>,
    stop: Arc<AtomicBool>,
    sync_error: ErrorSlot,
    names_error: ErrorSlot,
    retention_error: ErrorSlot,
    cache_status: Arc<Mutex<CacheStatus>>,
    cache_bytes: u64,
    retention: Option<(PathBuf, Duration)>,
    sources: Vec<Arc<dyn RecordSource + Send + Sync>>,
    record_peers: Option<SyncPeers>,
    commit_sources: Option<CommitSources>,
    dependency_error: ErrorSlot,
    peer_sync: Option<Scheduler>,
    inventory_error: ErrorSlot,
    discovery: Option<Discovery>,
    discovery_error: ErrorSlot,
}

impl Service {
    /// Control requests handled at once; more are refused with 503.
    const CONTROL_REQUESTS: usize = 8;
    const CAPABILITIES: [&str; 9] = [
        "status",
        "header-sync",
        "full-block-scan",
        "record-read",
        "object-status",
        "object-pin",
        "cache-prune",
        "wire-index",
        "names-index",
    ];

    pub fn bind(address: SocketAddr, token: String, engine: Arc<Engine>) -> Result<Self, Error> {
        if !address.ip().is_loopback()
            || token.len() != 64
            || !token.bytes().all(|c| c.is_ascii_hexdigit())
        {
            return Err(Error::Invalid(
                "service requires loopback and a 32-byte hexadecimal bearer token".into(),
            ));
        }
        let listener = TcpListener::bind(address)?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            token,
            engine,
            stop: Arc::new(AtomicBool::new(false)),
            sync_error: Arc::new(Mutex::new(None)),
            names_error: Arc::new(Mutex::new(None)),
            retention_error: Arc::new(Mutex::new(None)),
            cache_status: Arc::new(Mutex::new(CacheStatus::default())),
            cache_bytes: crate::cache::DEFAULT_CACHE_BYTES,
            retention: None,
            sources: Vec::new(),
            record_peers: None,
            commit_sources: None,
            dependency_error: Arc::new(Mutex::new(None)),
            peer_sync: None,
            inventory_error: Arc::new(Mutex::new(None)),
            discovery: None,
            discovery_error: Arc::new(Mutex::new(None)),
        })
    }

    pub fn address(&self) -> Result<SocketAddr, Error> {
        Ok(self.listener.local_addr()?)
    }

    pub fn shutdown_handle(&self) -> ShutdownHandle {
        ShutdownHandle {
            stop: Arc::clone(&self.stop),
        }
    }

    /// Explicit optional candidates for background object recovery. Merely
    /// configuring handles does not dial, fetch, change headers or scan coverage.
    pub fn with_record_sources(
        mut self,
        sources: Vec<Arc<dyn RecordSource + Send + Sync>>,
    ) -> Result<Self, Error> {
        if sources.len() > 1024 {
            return Err(Error::Invalid("too many configured record sources".into()));
        }
        self.sources = sources;
        Ok(self)
    }

    pub fn with_commit_sources(mut self, sources: CommitSources) -> Result<Self, Error> {
        if sources.is_empty() {
            return Err(Error::Invalid("commit source set is empty".into()));
        }
        let expected = self.engine.status()?.chain_genesis;
        let mut chains = sources.source_chains();
        chains.push(sources.chain());
        for chain in chains {
            let genesis = chain
                .genesis()
                .map_err(|error| Error::Invalid(error.to_string()))?
                .0;
            if genesis != expected {
                return Err(Error::Invalid(
                    "commit source belongs to another chain".into(),
                ));
            }
        }
        self.commit_sources = Some(sources);
        Ok(self)
    }

    /// Operational soft target, not persisted archive identity or a disk quota.
    /// Only run(true) schedules enforcement; run(false) remains worker-free.
    /// Zero requests eventual drainage of unprotected cache over bounded steps.
    pub fn with_cache_limit(mut self, bytes: u64) -> Result<Self, Error> {
        if bytes > i64::MAX as u64 {
            return Err(Error::Invalid("cache limit exceeds storage range".into()));
        }
        self.cache_bytes = bytes;
        Ok(self)
    }

    /// Configure background acquisition of every object, bounded only by the
    /// protocol geometry. Each round spends at most `slice` on one object; an
    /// unfinished object resumes from its stored records in a later round. The
    /// caller supplies an owned scratch directory. Only run(true) schedules it;
    /// pruned archives perform no automatic content acquisition.
    pub fn with_retention(mut self, scratch: PathBuf, slice: Duration) -> Result<Self, Error> {
        if slice.is_zero() {
            return Err(Error::Invalid(
                "object retention slice must be nonzero".into(),
            ));
        }
        self.retention = Some((scratch, slice));
        Ok(self)
    }

    /// Run foreground. Network acquisition is opt-in; status works offline.
    /// run(true) adds the sync and maintenance threads (see `worker`); stop
    /// joins both before this returns.
    pub fn run(self, synchronize: bool) -> Result<(), Error> {
        std::thread::scope(|scope| {
            if synchronize {
                scope.spawn(|| self.sync_loop());
                scope.spawn(|| self.maintenance_loop());
            }
            let outcome = self.accept_loop(scope);
            self.stop.store(true, Ordering::Release);
            outcome
        })
    }

    fn status(&self) -> Result<ServiceStatus, Error> {
        let (last_sync_error, last_sync_error_at) =
            RecordedError::split(read(&self.sync_error, "sync")?);
        let (last_retention_error, last_retention_error_at) =
            RecordedError::split(read(&self.retention_error, "retention")?);
        let (last_names_error, last_names_error_at) =
            RecordedError::split(read(&self.names_error, "Names")?);
        let (last_dependency_error, last_dependency_error_at) =
            RecordedError::split(read(&self.dependency_error, "dependency")?);
        let (last_inventory_error, last_inventory_error_at) =
            RecordedError::split(read(&self.inventory_error, "inventory")?);
        let (last_discovery_error, last_discovery_error_at) =
            RecordedError::split(read(&self.discovery_error, "discovery")?);
        let cache = self
            .cache_status
            .lock()
            .map_err(|_| Error::Invalid("cache status poisoned".into()))?
            .clone();
        let (last_cache_error, last_cache_error_at) = RecordedError::split(cache.error);
        let overlay = self.overlay_status()?;
        let mut status = ServiceStatus::default();
        // Archive first: a source records a lookup before its parent is
        // admitted, so a resolved parent's outcome is already in the table.
        status.archive = self.engine.status()?;
        let mut commit_sources = Vec::new();
        for sources in self.commit_sources.iter() {
            commit_sources.extend(sources.status()?);
        }
        status.last_sync_error = last_sync_error;
        status.last_sync_error_at = last_sync_error_at;
        status.last_retention_error = last_retention_error;
        status.last_retention_error_at = last_retention_error_at;
        status.last_names_error = last_names_error;
        status.last_names_error_at = last_names_error_at;
        status.last_dependency_error = last_dependency_error;
        status.last_dependency_error_at = last_dependency_error_at;
        status.last_inventory_error = last_inventory_error;
        status.last_inventory_error_at = last_inventory_error_at;
        status.last_discovery_error = last_discovery_error;
        status.last_discovery_error_at = last_discovery_error_at;
        status.commit_sources = commit_sources;
        status.cache_limit_bytes = self.cache_bytes;
        status.last_cache_error = last_cache_error;
        status.last_cache_error_at = last_cache_error_at;
        status.last_cache_maintenance = cache.receipt;
        status.overlay = overlay;
        status.chain_peers = self.engine.chain_peers()?;
        status.chain_target = self.engine.chain_target();
        status.retention_configured = self.retention.is_some();
        status.record_sources_configured = u64::try_from(self.record_source_count())
            .map_err(|error| Error::Invalid(format!("record source count: {error}")))?;
        status.capabilities = Self::CAPABILITIES.map(String::from).to_vec();
        Ok(status)
    }

    /// Each request runs on its own scoped thread, so a slow client or handler
    /// never delays another; past `CONTROL_REQUESTS` in flight, new ones get
    /// 503 at once. Stop joins the in-flight requests with the scope.
    fn accept_loop<'scope, 'env>(
        &'env self,
        scope: &'scope std::thread::Scope<'scope, 'env>,
    ) -> Result<(), Error> {
        let in_flight = Arc::new(AtomicUsize::new(0));
        while !self.stop.load(Ordering::Acquire) {
            match self.listener.accept() {
                Ok((mut stream, peer)) => {
                    if !peer.ip().is_loopback() {
                        continue;
                    }
                    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
                    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
                    if in_flight.fetch_add(1, Ordering::AcqRel) >= Self::CONTROL_REQUESTS {
                        in_flight.fetch_sub(1, Ordering::AcqRel);
                        // Best effort: a busy reply that cannot be written is moot.
                        let _ = respond(&mut stream, 503, json!({"error":"control service busy"}));
                        close_gracefully(&mut stream);
                        continue;
                    }
                    let in_flight = Arc::clone(&in_flight);
                    scope.spawn(move || {
                        // A malformed or disconnected client never stops the service.
                        let _ = self.request(&mut stream);
                        close_gracefully(&mut stream);
                        in_flight.fetch_sub(1, Ordering::AcqRel);
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(50))
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    fn request(&self, stream: &mut TcpStream) -> Result<(), Error> {
        let mut raw = Vec::new();
        let began = std::time::Instant::now();
        loop {
            if raw.len() == 8192 || began.elapsed() > Duration::from_secs(3) {
                return respond(stream, 400, json!({"error":"request header limit"}));
            }
            let mut byte = [0];
            if stream.read(&mut byte)? == 0 {
                return Ok(());
            }
            raw.push(byte[0]);
            if began.elapsed() > Duration::from_secs(3) {
                return respond(stream, 400, json!({"error":"request header deadline"}));
            }
            if raw.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let Ok(request) = std::str::from_utf8(&raw) else {
            return respond(stream, 400, json!({"error":"invalid header encoding"}));
        };
        let mut lines = request.split("\r\n");
        let first = lines.next().unwrap_or("");
        let mut authorized = false;
        let mut authorization_seen = false;
        for line in lines.filter(|line| !line.is_empty()) {
            let Some((name, value)) = line.split_once(':') else {
                return respond(stream, 400, json!({"error":"malformed header"}));
            };
            if name.eq_ignore_ascii_case("authorization") {
                if authorization_seen {
                    return respond(stream, 400, json!({"error":"duplicate authorization"}));
                }
                authorization_seen = true;
                authorized = value
                    .trim()
                    .strip_prefix("Bearer ")
                    .is_some_and(|value| equal_token(value, &self.token));
            }
            if name.eq_ignore_ascii_case("transfer-encoding")
                || (name.eq_ignore_ascii_case("content-length") && value.trim() != "0")
            {
                return respond(
                    stream,
                    400,
                    json!({"error":"this control endpoint accepts no request body"}),
                );
            }
        }
        if !authorized {
            return respond(stream, 401, json!({"error":"authentication required"}));
        }
        match first {
            line if (line.starts_with("GET /v1/names/") || line.starts_with("POST /v1/names/"))
                && line.ends_with(" HTTP/1.1") =>
            {
                let (method, path) = line[..line.len() - " HTTP/1.1".len()]
                    .split_once(' ')
                    .expect("matched HTTP method");
                let request = match names_request(method, path) {
                    Ok(Some(request)) => request,
                    Ok(None) => {
                        return respond(stream, 404, json!({"error":"unknown endpoint"}));
                    }
                    Err(_) => {
                        return respond(stream, 400, json!({"error":"invalid Names request"}));
                    }
                };
                // Reads never register or advance a registry. Mutations are
                // explicit one-shot commands under the Engine archive mutex.
                let result = match request {
                    NamesRequest::Status(genesis) => match self.engine.names_status(genesis) {
                        Ok(Some(status)) => serde_json::to_value(status).map_err(Error::from),
                        Ok(None) => {
                            return respond(
                                stream,
                                404,
                                json!({"error":"Names registry is not registered locally"}),
                            );
                        }
                        Err(error) => Err(error),
                    },
                    NamesRequest::Resolve(genesis, name) => self
                        .engine
                        .names_resolve(genesis, &name)
                        .and_then(|value| serde_json::to_value(value).map_err(Error::from)),
                    NamesRequest::Register(genesis, point) => self
                        .engine
                        .register_names(genesis, point)
                        .and_then(|value| serde_json::to_value(value).map_err(Error::from)),
                    NamesRequest::Step(genesis) => self
                        .engine
                        .names_step(genesis)
                        .and_then(|value| serde_json::to_value(value).map_err(Error::from)),
                    NamesRequest::View(genesis, limit, cursor) => self
                        .engine
                        .names_view(genesis, cursor.as_ref(), limit)
                        .and_then(|value| serde_json::to_value(value).map_err(Error::from)),
                };
                match result {
                    Ok(value) => respond(stream, 200, value),
                    Err(error) => control_error(stream, error),
                }
            }
            line if line.starts_with("GET /v1/wire?") && line.ends_with(" HTTP/1.1") => {
                let query = &line["GET /v1/wire?".len()..line.len() - " HTTP/1.1".len()];
                let (limit, cursor) = match wire_query(query) {
                    Ok(value) => value,
                    Err(_) => {
                        return respond(stream, 400, json!({"error":"invalid Wire page query"}));
                    }
                };
                match self.engine.wire_page(cursor.as_ref(), limit) {
                    Ok(page) => respond(stream, 200, serde_json::to_value(page)?),
                    Err(error) => control_error(stream, error),
                }
            }
            "POST /v1/cache/prune HTTP/1.1" => match self.engine.prune_cache() {
                Ok(records_removed) => respond(
                    stream,
                    200,
                    serde_json::to_value(crate::model::PruneReceipt { records_removed })?,
                ),
                Err(error) => control_error(stream, error),
            },
            line if line.starts_with("POST /v1/objects/") && line.ends_with(" HTTP/1.1") => {
                let path = &line["POST /v1/objects/".len()..line.len() - " HTTP/1.1".len()];
                let (id, pin) = match path.rsplit_once('/') {
                    Some((id, "pin")) => (id, true),
                    Some((id, "unpin")) => (id, false),
                    _ => return respond(stream, 404, json!({"error":"unknown endpoint"})),
                };
                if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return respond(stream, 400, json!({"error":"invalid root transaction ID"}));
                }
                let root = id
                    .parse()
                    .map_err(|e| Error::Invalid(format!("TXID: {e}")))?;
                // Engine serializes validation and mutation with pruning/reorg.
                // The acknowledgement describes this operation's commit point;
                // a lost response must not be treated as proof of no mutation.
                match self.engine.set_pin(root, pin) {
                    Ok(()) => respond(
                        stream,
                        200,
                        serde_json::to_value(crate::model::PinReceipt { root, pinned: pin })?,
                    ),
                    Err(error) => control_error(stream, error),
                }
            }
            line if line.starts_with("GET /v1/objects/") && line.ends_with(" HTTP/1.1") => {
                let id = &line["GET /v1/objects/".len()..line.len() - " HTTP/1.1".len()];
                if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return respond(stream, 400, json!({"error":"invalid root transaction ID"}));
                }
                let root = id
                    .parse()
                    .map_err(|e| Error::Invalid(format!("TXID: {e}")))?;
                match self.engine.object_status(root)? {
                    Some(status) => respond(stream, 200, serde_json::to_value(status)?),
                    None => respond(
                        stream,
                        404,
                        json!({"error":"no completed object recorded locally"}),
                    ),
                }
            }
            line if line.starts_with("GET /v1/records/") && line.ends_with(" HTTP/1.1") => {
                let id = &line["GET /v1/records/".len()..line.len() - " HTTP/1.1".len()];
                if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return respond(stream, 400, json!({"error":"invalid transaction ID"}));
                }
                let txid = id
                    .parse()
                    .map_err(|e| Error::Invalid(format!("TXID: {e}")))?;
                match self.engine.record(txid)? {
                    Some(record) => respond(stream, 200, serde_json::to_value(record)?),
                    None => respond(
                        stream,
                        404,
                        json!({"error":"record is not locally available in the selected view"}),
                    ),
                }
            }
            "GET /v1/status HTTP/1.1" => {
                respond(stream, 200, serde_json::to_value(self.status()?)?)
            }
            // Ascending by height; only the newest (highest) 1024 rows are returned.
            "GET /v1/overlay/omissions HTTP/1.1" => match self.engine.peer_omissions_latest(1024) {
                Ok(omissions) => respond(stream, 200, serde_json::to_value(omissions)?),
                Err(error) => control_error(stream, error),
            },
            "GET /v1/overlay/book HTTP/1.1" => match self.overlay_book() {
                Ok(Some(book)) => respond(stream, 200, serde_json::to_value(book)?),
                Ok(None) => respond(stream, 404, json!({"error":"overlay disabled"})),
                Err(error) => control_error(stream, error),
            },
            "POST /v1/shutdown HTTP/1.1" => {
                respond(stream, 200, json!({"stopping":true}))?;
                self.stop.store(true, Ordering::Release);
                Ok(())
            }
            _ => respond(stream, 404, json!({"error":"unknown endpoint"})),
        }
    }
}

enum NamesRequest {
    Status(bitcoin::Txid),
    Resolve(bitcoin::Txid, urma_names::name::Name),
    Register(bitcoin::Txid, crate::model::ChainPoint),
    Step(bitcoin::Txid),
    View(bitcoin::Txid, u32, Option<crate::names::NamesCursor>),
}

fn names_request(method: &str, path: &str) -> Result<Option<NamesRequest>, Error> {
    let invalid = || Error::Invalid("invalid Names request".into());
    let path = path.strip_prefix("/v1/names/").ok_or_else(invalid)?;
    let (id, operation) = path.split_once('/').unwrap_or((path, ""));
    if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid());
    }
    let genesis = id.parse().map_err(|_| invalid())?;
    let request = match (method, operation) {
        ("GET", "") => NamesRequest::Status(genesis),
        ("GET", operation) if operation.starts_with("view?") => {
            let (limit, cursor) = names_view_query(&operation["view?".len()..])?;
            if cursor.as_ref().is_some_and(|c| c.genesis != genesis) {
                return Err(invalid());
            }
            NamesRequest::View(genesis, limit, cursor)
        }
        ("GET", operation) if operation.starts_with("resolve/") => {
            // Canonical Names V0 uses only ASCII letters, digits and hyphens;
            // do not normalize, percent-decode or accept alternate spellings.
            let name = urma_names::name::Name::parse(&operation["resolve/".len()..])
                .map_err(|_| invalid())?;
            NamesRequest::Resolve(genesis, name)
        }
        ("POST", "step") => NamesRequest::Step(genesis),
        ("POST", operation) if operation.starts_with("register?") => {
            let mut height = None;
            let mut hash = None;
            for field in operation["register?".len()..].split('&') {
                match field.split_once('=') {
                    Some(("height", value)) if height.is_none() => {
                        if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                            return Err(invalid());
                        }
                        let value: u64 = value.parse().map_err(|_| invalid())?;
                        if value > i64::MAX as u64 {
                            return Err(invalid());
                        }
                        height = Some(value);
                    }
                    Some(("hash", value)) if hash.is_none() => {
                        if value.len() != 64 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
                            return Err(invalid());
                        }
                        hash = Some(value.parse().map_err(|_| invalid())?);
                    }
                    _ => return Err(invalid()),
                }
            }
            NamesRequest::Register(
                genesis,
                crate::model::ChainPoint {
                    height: height.ok_or_else(invalid)?,
                    hash: hash.ok_or_else(invalid)?,
                },
            )
        }
        _ => return Ok(None),
    };
    Ok(Some(request))
}

fn names_view_query(query: &str) -> Result<(u32, Option<crate::names::NamesCursor>), Error> {
    let invalid = || Error::Invalid("invalid Names view query".into());
    let mut limit = None;
    let mut cursor = None;
    for field in query.split('&') {
        match field.split_once('=') {
            Some(("limit", value)) if limit.is_none() => {
                if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(invalid());
                }
                let value: u32 = value.parse().map_err(|_| invalid())?;
                if !(1..=crate::names::MAX_NAMES_PAGE).contains(&value) {
                    return Err(invalid());
                }
                limit = Some(value);
            }
            Some(("cursor", value)) if cursor.is_none() => {
                if value.len() > 1024 {
                    return Err(invalid());
                }
                let bytes = hex::decode(value).map_err(|_| invalid())?;
                let decoded: crate::names::NamesCursor = serde_json::from_slice(&bytes)?;
                if decoded.snapshot.instance.len() != 32
                    || !decoded
                        .snapshot
                        .instance
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit())
                {
                    return Err(invalid());
                }
                cursor = Some(decoded);
            }
            _ => return Err(invalid()),
        }
    }
    Ok((limit.ok_or_else(invalid)?, cursor))
}

fn wire_query(query: &str) -> Result<(u32, Option<crate::projection::WireCursor>), Error> {
    let invalid = || Error::Invalid("invalid Wire page query".into());
    let mut limit = None;
    let mut cursor = None;
    for field in query.split('&') {
        match field.split_once('=') {
            Some(("limit", value)) if limit.is_none() => {
                if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(invalid());
                }
                let value: u32 = value.parse().map_err(|_| invalid())?;
                if !(1..=crate::projection::MAX_WIRE_PAGE).contains(&value) {
                    return Err(invalid());
                }
                limit = Some(value);
            }
            Some(("cursor", value)) if cursor.is_none() => {
                if value.len() > 1024 {
                    return Err(invalid());
                }
                let bytes = hex::decode(value).map_err(|_| invalid())?;
                let decoded: crate::projection::WireCursor = serde_json::from_slice(&bytes)?;
                if decoded.snapshot.instance.len() != 32
                    || !decoded
                        .snapshot
                        .instance
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit())
                    || decoded.height > i64::MAX as u64
                {
                    return Err(invalid());
                }
                cursor = Some(decoded);
            }
            _ => return Err(invalid()),
        }
    }
    Ok((limit.ok_or_else(invalid)?, cursor))
}

fn control_error(stream: &mut TcpStream, error: Error) -> Result<(), Error> {
    match error {
        Error::Invalid(message) => respond(stream, 422, json!({"error":message})),
        error @ (Error::Busy(_)
        | Error::PeerBusy
        | Error::Transport(_)
        | Error::DialDenied(_)
        | Error::Stale) => respond(stream, 409, json!({"error":error.to_string()})),
        // Do not expose database internals or local filesystem paths over HTTP.
        _ => respond(
            stream,
            500,
            json!({"error":"local archive operation failed"}),
        ),
    }
}

fn equal_token(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.bytes()
        .zip(right.bytes())
        .fold(0u8, |different, (a, b)| different | (a ^ b))
        == 0
}

/// Closing a socket with unread request bytes makes the kernel send RST and
/// drop any reply still queued, so a client that sent more than we read (an
/// oversized header) could lose its 400. Half-close first so the reply ends in
/// FIN, then drain what the client still sends, bounded in bytes and time.
fn close_gracefully(stream: &mut TcpStream) {
    const DRAIN_BYTES: usize = 64 * 1024;
    const DRAIN_TIME: Duration = Duration::from_millis(200);
    if stream.shutdown(Shutdown::Write).is_err() {
        return;
    }
    let deadline = Instant::now() + DRAIN_TIME;
    let mut drained = 0;
    let mut chunk = [0; 4096];
    while drained < DRAIN_BYTES {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || stream.set_read_timeout(Some(left)).is_err() {
            return;
        }
        match stream.read(&mut chunk) {
            Ok(0) => return,
            Ok(read) => drained += read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_closed_or_timed_out) => return,
        }
    }
}

fn respond(stream: &mut TcpStream, code: u16, body: serde_json::Value) -> Result<(), Error> {
    let body = serde_json::to_vec(&body)?;
    if body.len() > 32 * 1024 * 1024 {
        return respond(
            stream,
            413,
            json!({"error":"record exceeds local API response limit"}),
        );
    }
    let reason = match code {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        409 => "Conflict",
        413 => "Content Too Large",
        422 => "Unprocessable Content",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Not Found",
    };
    write!(
        stream,
        "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(&body)?;
    Ok(())
}

/// New private service token, never a wallet/signing/decryption key. Existing
/// files are not overwritten.
pub fn create_token(path: &Path) -> Result<(), Error> {
    let mut file = urma_io::create_file(path, Access::Private)?;
    write_token(&mut file)?;
    file.sync_all()?;
    Ok(())
}

/// 32 random bytes, hex encoded, into a file whose access the caller chose.
pub fn write_token(file: &mut File) -> Result<(), Error> {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    file.write_all(hex::encode(bytes).as_bytes())?;
    Ok(())
}
