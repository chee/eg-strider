use crate::EventRunIndex;
use std::ops::Range;

/// A stretch of history between two critical versions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HistoryRange {
    /// Runs nothing is concurrent with: their events apply as recorded.
    Linear(Range<EventRunIndex>),
    /// Runs with concurrency among them, which have to be merged.
    Concurrent(Range<EventRunIndex>),
}

impl HistoryRange {
    /// Returns the runs in the range.
    pub fn runs(&self) -> &Range<EventRunIndex> {
        match self {
            HistoryRange::Linear(runs) | HistoryRange::Concurrent(runs) => runs,
        }
    }
}
