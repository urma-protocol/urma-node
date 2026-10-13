//! Peer synchronization bookkeeping. Coverage is a local claim about which
//! peer listings were fully admitted; full-block scans remain the audit.
use crate::{
    Error,
    engine::Engine,
    model::{ChainPoint, Omission},
};

impl Engine {
    /// Record that `peer` (its PeerId string) listed every placement in
    /// selected [from, to] and each listed placement was admitted, already
    /// held, or answered Missing (listed-only).
    /// `generation` is the one observed before fetching; a reorg since is Stale.
    /// admit_record keeps multipart children even in Pruned mode, so synced
    /// components stay until cache maintenance evicts them.
    pub fn record_peer_coverage(
        &self,
        peer: &str,
        from: u64,
        to: u64,
        generation: u64,
    ) -> Result<(), Error> {
        self.archive()?
            .record_peer_coverage(peer, from, to, generation)
    }

    /// First height after the peer's contiguous coverage from app start.
    pub fn peer_cursor(&self, peer: &str) -> Result<u64, Error> {
        self.archive()?.peer_cursor(peer, 0)
    }

    /// As `peer_cursor`, starting at the peer's own scan floor when that is
    /// above app start: heights below it are never asked of this peer.
    pub fn peer_cursor_from(&self, peer: &str, floor: u64) -> Result<u64, Error> {
        self.archive()?.peer_cursor(peer, floor)
    }

    pub fn peer_synced_through(&self) -> Result<Option<ChainPoint>, Error> {
        self.archive()?.peer_synced_through()
    }

    pub fn omitting_peers(&self) -> Result<Vec<String>, Error> {
        self.archive()?.omitting_peers()
    }

    pub fn peer_omissions(&self) -> Result<Vec<Omission>, Error> {
        self.archive()?.peer_omissions()
    }

    /// The `limit` highest selected omissions, ascending; bounded in SQL.
    pub fn peer_omissions_latest(&self, limit: u32) -> Result<Vec<Omission>, Error> {
        self.archive()?.peer_omissions_latest(limit)
    }

    /// Chain peers the source holds now; 0 without a source (offline).
    pub fn chain_peers(&self) -> Result<u64, Error> {
        match &self.source {
            Some(source) => source.chain_peers(),
            None => Ok(0),
        }
    }

    /// Absolute header-sync target; `None` while unknown or offline.
    pub fn chain_target(&self) -> Option<u64> {
        self.source
            .as_ref()
            .and_then(|source| source.chain_target())
    }
}
