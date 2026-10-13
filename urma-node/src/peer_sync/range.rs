//! One claimed inventory range of one peer: how it is opened from the
//! persisted cursor and how each page is checked before it is walked.
use super::Scheduler;
use crate::{
    Error,
    engine::Engine,
    peer_protocol::{InventoryPage, Placement},
};

/// One claimed range being walked page by page; lives only in memory.
pub(super) struct Range {
    pub(super) from: u64,
    pub(super) to: u64,
    pub(super) after: Option<Placement>,
    pub(super) entries: Vec<Placement>,
    pub(super) next: usize,
    pub(super) more: bool,
    pub(super) scanned_through: u64,
    pub(super) generation: u64,
    /// Consecutive Missing answers for records this range advertised.
    pub(super) missing: u32,
}

/// What a page means for its range.
pub(super) enum Accepted {
    /// Entries installed; keep walking.
    Walk,
    /// The peer has not scanned the range's start yet.
    Unscanned,
    /// The peer never scanned below this height: its page says nothing about
    /// the range's start, so the range reopens there and claims nothing below.
    Floor(u64),
}

/// Whether a peer has a range left to walk.
pub(super) enum Opened {
    Range(Range),
    /// Covered through our selected tip (or no selected view yet).
    Synced,
    /// The peer's archive starts above our tip: it has nothing to offer.
    AboveTip,
}

impl Opened {
    pub(super) fn range(self) -> Option<Range> {
        match self {
            Opened::Range(range) => Some(range),
            Opened::Synced | Opened::AboveTip => None,
        }
    }
}

/// The next unclaimed range for peer `id` at or above its `floor`, bounded by
/// the local selected tip. The range keeps the generation observed here for
/// its whole lifetime, so a reorg between rounds makes its coverage claim
/// Stale instead of mislabelled.
pub(super) fn open(id: &str, engine: &Engine, floor: u64) -> Result<Opened, Error> {
    let status = engine.status()?;
    let Some(tip) = status.selected_tip else {
        return Ok(Opened::Synced);
    };
    if floor > tip.height {
        return Ok(Opened::AboveTip);
    }
    let from = engine.peer_cursor_from(id, floor)?;
    if from > tip.height {
        return Ok(Opened::Synced);
    }
    let span_end = from
        .checked_add(Scheduler::RANGE_HEIGHTS - 1)
        .ok_or_else(|| Error::Invalid("peer cursor overflows the height range".into()))?;
    Ok(Opened::Range(Range {
        from,
        to: tip.height.min(span_end),
        after: None,
        entries: Vec::new(),
        next: 0,
        more: true,
        scanned_through: 0,
        generation: status.generation,
        missing: 0,
    }))
}

/// Install a page. Coverage is only ever claimed inside the peer's own
/// scanned interval [`scanned_from`, `scanned_through`]: a page whose floor is
/// above the range start installs nothing. Entries must be strictly ordered
/// after the page cursor and inside the requested range; anything else is a
/// peer error. Entries above the peer's own scanned prefix (blocks still
/// waiting on a dependency) are dropped without blame: they are outside what
/// the page can claim.
pub(super) fn accept(range: &mut Range, page: InventoryPage) -> Result<Accepted, Error> {
    if page.scanned_from > range.from {
        return Ok(Accepted::Floor(page.scanned_from));
    }
    let Some(scanned_through) = page.scanned_through.filter(|height| *height >= range.from) else {
        return Ok(Accepted::Unscanned);
    };
    let limit = usize::try_from(Scheduler::PAGE_ENTRIES)
        .map_err(|error| Error::Invalid(format!("page limit: {error}")))?;
    if page.entries.len() > limit || (page.more && page.entries.is_empty()) {
        return Err(Error::Invalid("peer inventory page size is invalid".into()));
    }
    let mut previous = range.after;
    for entry in &page.entries {
        if entry.height < range.from
            || entry.height > range.to
            || previous
                .is_some_and(|last| (entry.height, entry.position) <= (last.height, last.position))
        {
            return Err(Error::Invalid(
                "peer inventory entry is out of range or order".into(),
            ));
        }
        previous = Some(*entry);
    }
    range.after = previous;
    range.entries = page
        .entries
        .into_iter()
        .filter(|entry| entry.height <= scanned_through)
        .collect();
    range.next = 0;
    range.more = page.more;
    range.scanned_through = scanned_through;
    Ok(Accepted::Walk)
}
