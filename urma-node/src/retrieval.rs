//! On-demand content acquisition. Sources supply untrusted packages; the local
//! engine supplies the independently selected chain view. Missing peers do not
//! imply missing chain data: a known locator can fall back to full witness blocks.
use crate::{Error, engine::Engine, model::RecordPackage};
use bitcoin::Txid;
use std::{
    collections::BTreeMap,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
use urma_core::multipart::{Geometry, VerifiedRecord};
use urma_runtime::multipart::{
    Candidate, FetchError, MultipartSource, RecordRequest, RecoveredObject, RecoveryError,
    RecoveryLimits,
};

/// A data source, not a chain verifier or completeness authority. Implementations
/// must bound their own I/O; the caller's deadline is checked between callbacks.
pub trait RecordSource {
    fn fetch(&self, txid: Txid) -> Result<Option<RecordPackage>, Error>;
}

/// Explicit acquisition budgets, independent of protocol geometry limits.
#[derive(Clone, Copy)]
pub struct FetchLimits {
    pub sources: usize,
    pub timeout: Duration,
}
impl Default for FetchLimits {
    fn default() -> Self {
        Self {
            sources: 4,
            timeout: Duration::from_secs(30),
        }
    }
}

fn verify_cached(
    engine: &Engine,
    expected: Txid,
    package: &RecordPackage,
) -> Result<VerifiedRecord, Error> {
    let chain = engine.chain()?;
    let reveal = urma_chain::decode::transaction(&package.reveal.transaction, chain)
        .map_err(|e| Error::Invalid(e.to_string()))?;
    let commit = urma_chain::decode::transaction(&package.commit, chain)
        .map_err(|e| Error::Invalid(e.to_string()))?;
    VerifiedRecord::verify(expected, &reveal, &commit).map_err(|e| Error::Invalid(e.to_string()))
}

/// Fetch one structurally valid V0 record. This does not check a signed parent
/// reference hash: multipart reconstruction performs that check AFTER proof.
/// A returned record is relative to the local selected view at retrieval time.
pub fn fetch_record(
    engine: &Engine,
    txid: Txid,
    sources: &[&dyn RecordSource],
    limits: FetchLimits,
) -> Result<Option<VerifiedRecord>, Error> {
    let began = Instant::now();
    let deadline = began
        .checked_add(limits.timeout)
        .ok_or_else(|| Error::Invalid("record deadline overflow".into()))?;
    crate::engine::check_deadline(deadline)?;
    if let Some(record) = engine.record(txid)? {
        let verified = verify_cached(engine, txid, &record.package)?;
        crate::engine::check_deadline(deadline)?;
        return Ok(Some(verified));
    }
    let mut last = None;
    for source in sources.iter().take(limits.sources) {
        if began.elapsed() >= limits.timeout {
            return Err(Error::Busy("record acquisition deadline".into()));
        }
        let package = match source.fetch(txid) {
            Ok(Some(package)) => package,
            Ok(None) => continue,
            Err(error) => {
                last = Some(error);
                continue;
            }
        };
        // A package that arrives after the deadline is still verified and
        // persisted: acquisition is resumable, so fetched work is never discarded.
        let late = began.elapsed() >= limits.timeout;
        // Bind the requested identifier before admitting any package. Valid data
        // for an unrelated TXID cannot satisfy this request or pollute its cache.
        match verify_cached(engine, txid, &package) {
            Err(error) if late => return Err(error),
            Err(error) => {
                last = Some(error);
                continue;
            }
            Ok(verified) => {
                match engine.admit_record_before(package, (!late).then_some(deadline)) {
                    Ok(_) => return Ok(Some(verified)),
                    Err(error) => {
                        // A peer's stale block claim must not veto another source.
                        // Admission still checks tickets; recover checks its epoch.
                        last = Some(error);
                    }
                }
            }
        }
    }
    if began.elapsed() >= limits.timeout {
        return Err(Error::Busy("record acquisition deadline".into()));
    }
    let fallback = engine.retrieve_located_record_before(txid, deadline);
    crate::engine::check_deadline(deadline)?;
    match fallback {
        Ok(Some(record)) => {
            if began.elapsed() >= limits.timeout {
                return Err(Error::Busy("record acquisition deadline".into()));
            }
            let verified = verify_cached(engine, txid, &record.package)?;
            crate::engine::check_deadline(deadline)?;
            Ok(Some(verified))
        }
        Err(error) => Err(error),
        Ok(None) => match last {
            Some(error) => Err(error),
            None => Ok(None),
        },
    }
}

/// How one object acquisition spends time. `slice` bounds network fetches only:
/// records already stored re-verify outside it, and a fetch is started only
/// while network time remains, so every attempt that reaches a missing record
/// fetches at least one. Each fetch is bounded by its own `fetch` limits.
#[derive(Clone, Copy)]
pub(crate) struct Pace {
    limits: RecoveryLimits,
    slice: Duration,
    fetch: FetchLimits,
}

impl Pace {
    fn on_demand(limits: RecoveryLimits) -> Self {
        Self {
            limits,
            slice: limits.timeout,
            fetch: FetchLimits {
                sources: limits.attempts.max(1) as usize,
                timeout: limits.timeout,
            },
        }
    }

    /// Background unpruned acquisition: no size budget beyond the protocol
    /// geometry and no overall deadline; only network time is sliced. Every
    /// supplied source is asked in order, so a record one peer lacks or serves
    /// badly still comes from the next; the caller bounds the source list.
    pub(crate) fn background(slice: Duration) -> Self {
        Self {
            limits: RecoveryLimits {
                max_payload_bytes: Geometry::MAX_OBJECT_BYTES,
                max_nodes: Geometry::MAX_NODES,
                attempts: 1,
                timeout: RecoveryLimits::TIMEOUT,
            },
            slice,
            fetch: FetchLimits {
                sources: usize::MAX,
                ..FetchLimits::default()
            },
        }
    }
}

/// A completed object, or an attempt whose network slice ran out. A sliced
/// attempt keeps every record it fetched; the next attempt continues from them.
pub(crate) enum Recovery {
    Complete(RecoveredObject),
    Sliced(RecoveryError),
}

fn completed(recovery: Recovery) -> Result<RecoveredObject, Error> {
    match recovery {
        Recovery::Complete(object) => Ok(object),
        Recovery::Sliced(cause) => {
            Err(Error::Busy(format!("object acquisition deadline: {cause}")))
        }
    }
}

/// Recover an object without exporting partial output. This reuses the existing
/// root/leaf/data signed-reference and final length/digest checks. Reorg during
/// recovery invalidates the result; cached immutable bytes remain reusable.
/// The scratch directory belongs to the caller. No plaintext decryption occurs.
pub fn recover(
    engine: &Engine,
    root: Txid,
    sources: &[&dyn RecordSource],
    limits: RecoveryLimits,
    scratch: &Path,
) -> Result<RecoveredObject, Error> {
    completed(recover_inner(
        engine,
        root,
        sources,
        Pace::on_demand(limits),
        scratch,
        false,
        &AtomicBool::new(false),
    )?)
}

/// Recover and protect all exact members in the same completion transaction.
/// On failure, no new completion or pin is published; independently verified
/// partial cache entries may remain. No user decryption key is required.
pub fn recover_and_pin(
    engine: &Engine,
    root: Txid,
    sources: &[&dyn RecordSource],
    limits: RecoveryLimits,
    scratch: &Path,
) -> Result<RecoveredObject, Error> {
    completed(recover_inner(
        engine,
        root,
        sources,
        Pace::on_demand(limits),
        scratch,
        true,
        &AtomicBool::new(false),
    )?)
}

pub(crate) fn recover_inner(
    engine: &Engine,
    root: Txid,
    sources: &[&dyn RecordSource],
    pace: Pace,
    scratch: &Path,
    pin: bool,
    cancelled: &AtomicBool,
) -> Result<Recovery, Error> {
    check_cancelled(cancelled)?;
    let generation = engine.status()?.generation;
    let began = Instant::now();
    let deadline = began
        .checked_add(pace.limits.timeout)
        .ok_or_else(|| Error::Invalid("object deadline overflow".into()))?;
    let root = fetch_record(engine, root, sources, pace.fetch)?
        .ok_or_else(|| Error::Invalid("root is not available from the supplied sources".into()))?;
    check_cancelled(cancelled)?;
    let mut remaining = pace.limits;
    remaining.timeout = pace.limits.timeout.saturating_sub(began.elapsed());
    let mut adapter = Adapter {
        engine,
        sources,
        pace,
        began,
        network: Duration::ZERO,
        sliced: false,
        members: BTreeMap::from([(root.txid(), crate::archive::ObjectMember::from(&root))]),
        cancelled,
    };
    let object = match urma_runtime::multipart::reconstruct(&root, &mut adapter, remaining, scratch)
    {
        Ok(object) => object,
        Err(cause) if adapter.sliced => return Ok(Recovery::Sliced(cause)),
        Err(error) => return Err(Error::Source(Box::new(error))),
    };
    check_cancelled(cancelled)?;
    // Completion atomically rechecks the epoch and retained identities. A
    // concurrent prune may refuse completion, never create false availability.
    engine.complete_object(
        &object,
        generation,
        &adapter.members.into_values().collect::<Vec<_>>(),
        pin,
        deadline,
        cancelled,
    )?;
    Ok(Recovery::Complete(object))
}

fn source_error(message: String) -> FetchError {
    FetchError::Source(urma_runtime::error::Error::Io(std::io::Error::other(
        message,
    )))
}

struct Adapter<'a> {
    engine: &'a Engine,
    sources: &'a [&'a dyn RecordSource],
    pace: Pace,
    began: Instant,
    network: Duration,
    sliced: bool,
    members: BTreeMap<Txid, crate::archive::ObjectMember>,
    cancelled: &'a AtomicBool,
}

impl Adapter<'_> {
    /// Stored records re-verify without spending the network slice.
    fn stored(&self, txid: Txid) -> Result<Option<VerifiedRecord>, Error> {
        self.engine
            .record(txid)?
            .map(|record| verify_cached(self.engine, txid, &record.package))
            .transpose()
    }

    fn fetch_missing(&mut self, txid: Txid) -> Result<VerifiedRecord, FetchError> {
        if self.network >= self.pace.slice {
            self.sliced = true;
            return Err(source_error("object acquisition slice ended".into()));
        }
        let timeout = self.pace.fetch.timeout.min(
            self.pace
                .limits
                .timeout
                .saturating_sub(self.began.elapsed()),
        );
        let started = Instant::now();
        // Source-level proof failures have already tried the bounded source list.
        // A fully authenticated wrong reference hash remains terminal in reconstruct.
        let fetched = fetch_record(
            self.engine,
            txid,
            self.sources,
            FetchLimits {
                timeout,
                ..self.pace.fetch
            },
        );
        self.network += started.elapsed();
        match fetched {
            Ok(Some(record)) => Ok(record),
            Ok(None) => Err(FetchError::Unavailable),
            Err(error) => Err(source_error(error.to_string())),
        }
    }
}

impl MultipartSource for Adapter<'_> {
    fn fetch(
        &mut self,
        request: &RecordRequest,
        _rejected: &[String],
    ) -> Result<Candidate, FetchError> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(source_error("object retention cancelled".into()));
        }
        let txid = request.reference.txid;
        let record = match self.stored(txid) {
            Ok(Some(record)) => record,
            Ok(None) => self.fetch_missing(txid)?,
            Err(error) => return Err(source_error(error.to_string())),
        };
        self.members
            .insert(record.txid(), crate::archive::ObjectMember::from(&record));
        Ok(Candidate {
            origin: "node-selected-view".into(),
            record,
        })
    }
}

fn check_cancelled(cancelled: &AtomicBool) -> Result<(), Error> {
    if cancelled.load(Ordering::Acquire) {
        Err(Error::Busy("object retention cancelled".into()))
    } else {
        Ok(())
    }
}
