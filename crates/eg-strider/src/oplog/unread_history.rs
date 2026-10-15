use std::ops::Range;
use strider_graph::EventIndex;

/// Index of a part of saved history.
pub(crate) type HistoryPartIndex = usize;

/// History still as saved, in parts: the events before the first that the log holds.
pub(crate) struct UnreadHistory {
    /// The file the log was loaded from.
    pub(super) bytes: Vec<u8>,
    /// Where each part is in `bytes`.
    pub(super) parts: Vec<Range<usize>>,
    /// Each part's first event, then the total number of events.
    pub(super) starts: Vec<EventIndex>,
    /// That history's frontier.
    pub frontier: Vec<EventIndex>,
}

impl UnreadHistory {
    /// Finds the part holding an event.
    pub fn part_holding(&self, event: EventIndex) -> HistoryPartIndex {
        self.starts.partition_point(|&start| start <= event) - 1
    }
}
