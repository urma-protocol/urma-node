//! Background work of run(true), on two threads so neither waits on the other.
//! The sync thread selects headers and scans blocks back to back while behind
//! the selected tip, pausing only after a round without scan progress. The
//! maintenance thread runs discovery, peer inventory, parent acquisition,
//! Names, object retention and cache upkeep, pausing after every round.
use super::Service;
use crate::{
    engine_sync::CatchUp, recorded::RecordedError, recorded::record, retrieval::RecordSource,
};
use std::{sync::atomic::Ordering, time::Duration};

impl Service {
    /// Most blocks one sync round scans before it re-selects headers.
    pub const CATCH_UP_BLOCKS: u64 = 256;
    /// Longest scanning stretch of one sync round.
    pub const CATCH_UP_TIME: Duration = Duration::from_secs(5);
    const PAUSE_STEPS: u32 = 20;
    const PAUSE_STEP: Duration = Duration::from_millis(100);

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    /// Two seconds in short steps, ending early on stop.
    fn pause(&self) {
        for _ in 0..Self::PAUSE_STEPS {
            if self.stopped() {
                return;
            }
            std::thread::sleep(Self::PAUSE_STEP);
        }
    }

    pub(super) fn sync_loop(&self) {
        while !self.stopped() {
            if !self.sync_round() {
                self.pause();
            }
        }
    }

    pub(super) fn maintenance_loop(&self) {
        while !self.stopped() {
            self.maintenance_round();
            self.pause();
        }
    }

    /// Headers, then a bounded catch-up scan and one cached parent retry.
    /// Returns whether any block was scanned.
    fn sync_round(&self) -> bool {
        let catch_up = match self.engine.synchronize_headers_until(&self.stop) {
            Ok(_) => {
                self.engine
                    .scan_catch_up(&self.stop, Self::CATCH_UP_BLOCKS, Self::CATCH_UP_TIME)
            }
            Err(error) => CatchUp {
                scanned: 0,
                outcome: Err(error),
            },
        };
        let result = catch_up.outcome.and_then(|status| {
            if !self.stopped() {
                self.engine.resolve_cached_dependency()?;
            }
            Ok(status)
        });
        record(&self.sync_error, &result);
        catch_up.scanned > 0
    }

    fn maintenance_round(&self) {
        // Discovery keeps connections and the book fresh before inventory
        // sync uses them; both are independent of the sync thread.
        self.discover_peers();
        self.sync_next_inventory();
        self.acquire_next_dependency();
        // Registered projections use already scanned data. They never
        // create registrations or acquire missing history.
        if !self.stopped() {
            let result = self.engine.maintain_next_names(&self.stop);
            record(&self.names_error, &result);
        }
        if !self.stopped() {
            self.retain_next_object();
        }
        // Local cache upkeep never opens a source.
        if !self.stopped() {
            let result = self.engine.maintain_cache(self.cache_bytes, &self.stop);
            if let Ok(mut status) = self.cache_status.lock() {
                match result {
                    Ok(receipt) => {
                        status.receipt = Some(receipt);
                        status.error = None;
                    }
                    Err(cause) => {
                        status.error = Some(RecordedError::now(cause));
                    }
                }
            }
        }
    }

    /// One object slice from the last selected view; a sync failure does not
    /// suppress it, and its error stays separately visible.
    fn retain_next_object(&self) {
        let Some((scratch, slice)) = &self.retention else {
            return;
        };
        let owned = self.record_sources();
        let sources: Vec<&dyn RecordSource> = owned
            .iter()
            .map(|source| -> &dyn RecordSource { source.as_ref() })
            .collect();
        let result = self
            .engine
            .retain_next_object(&sources, *slice, scratch, &self.stop);
        record(&self.retention_error, &result);
    }

    fn acquire_next_dependency(&self) {
        let Some(sources) = &self.commit_sources else {
            return;
        };
        if self.stopped() {
            return;
        }
        let result = self.engine.resolve_next_dependency(sources, &self.stop);
        if self.stopped() {
            return;
        }
        record(&self.dependency_error, &result);
    }
}
