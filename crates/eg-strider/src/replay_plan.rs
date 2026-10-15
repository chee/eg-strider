use crate::crdt_records::RecordId;
use crate::operation_run_slice::OperationRunSlice;
use crate::oplog::{OpLog, OperationRunIndex};
use std::ops::Range;
use strider_graph::{EventIndex, EventRunIndex, HistoryRange};

/// What a concurrent range's critical version keeps once the range has been through the CRDT
/// records, so that a later replay can place its events by record.
#[derive(Default)]
pub(crate) struct ResolvedOrigins {
    /// How many of the range's events, from its start, `resolved` covers.
    pub(crate) events: u32,
    /// Records, operation run by operation run in log order. An insert: its first character's
    /// left origin, then the right origin all its characters share. A delete: per group of
    /// consecutive targets, the first and the count, with
    /// [`BACKWARD_TARGETS`](crate::replay::BACKWARD_TARGETS) if they count down.
    pub(crate) resolved: Vec<RecordId>,
}

/// The history's shape, and what earlier replays of a log worked out, valid while the log has
/// the same number of events.
pub(crate) struct ReplayPlan {
    pub(crate) events: usize,
    /// The first event of each range.
    pub(crate) range_starts: Vec<EventIndex>,
    pub(crate) ranges: Vec<HistoryRange>,
    /// Per event run, the operation run holding its first event; then the number of operation
    /// runs.
    pub(crate) first_op_of_run: Vec<OperationRunIndex>,
    /// Per range, what its starting critical version keeps: nothing unless it is a concurrent
    /// range that went through the CRDT records.
    pub(crate) resolved: Vec<ResolvedOrigins>,
    /// How many positions the concurrent ranges took last time, to reserve next time.
    pub(crate) room: usize,
}

impl ReplayPlan {
    /// Analyses the log, taking over what the `previous` plan resolved for concurrent ranges that
    /// start at the same event.
    pub(crate) fn new(log: &OpLog, mut previous: Option<ReplayPlan>) -> Self {
        let room = previous.as_ref().map_or(0, |previous| previous.room);
        let runs = log.graph.runs();
        let ranges = log.graph.split_at_critical_versions();
        let range_starts = ranges.iter().map(|range| runs.start(range.runs().start)).collect();
        let first_op_of_run = log.ops_containing((0..=runs.len()).map(|run| runs.start(run)));
        let resolved = ranges
            .iter()
            .map(|range| match (range, &mut previous) {
                (HistoryRange::Concurrent(concurrent), Some(previous)) => previous.take_resolved(runs.start(concurrent.start)),
                _ => ResolvedOrigins::default(),
            })
            .collect();
        ReplayPlan { events: log.len(), range_starts, ranges, first_op_of_run, resolved, room }
    }

    /// Takes what was resolved for the concurrent range starting at `start`, if anything.
    pub(crate) fn take_resolved(&mut self, start: EventIndex) -> ResolvedOrigins {
        match self.range_starts.binary_search(&start) {
            Ok(at) => std::mem::take(&mut self.resolved[at]),
            Err(_) => ResolvedOrigins::default(),
        }
    }

    /// Iterates over the operations of a range of runs, cutting any that run past either end.
    #[inline]
    pub(crate) fn ops<'a>(&self, log: &'a OpLog, run_range: &Range<EventRunIndex>) -> impl DoubleEndedIterator<Item = OperationRunSlice> + 'a {
        self.ops_from(log, run_range, log.graph.runs().start(run_range.start))
    }

    /// Iterates over the operations of a range of runs from event `from`, which is in it.
    #[inline]
    pub(crate) fn ops_from<'a>(
        &self,
        log: &'a OpLog,
        run_range: &Range<EventRunIndex>,
        from: EventIndex,
    ) -> impl DoubleEndedIterator<Item = OperationRunSlice> + 'a {
        log.ops(self.op_at(log, run_range, from)..self.first_op_of_run[run_range.end as usize], from..log.graph.runs().start(run_range.end))
    }

    /// Returns the operation run holding event `from` of a range of runs.
    pub(crate) fn op_at(&self, log: &OpLog, run_range: &Range<EventRunIndex>, from: EventIndex) -> OperationRunIndex {
        if from == log.graph.runs().start(run_range.start) {
            self.first_op_of_run[run_range.start as usize]
        } else {
            log.op_containing(from)
        }
    }

    /// Counts the concurrent ranges that the last replay could not merge by transforming alone.
    pub(crate) fn ranges_using_crdt_records(&self) -> usize {
        self.resolved.iter().filter(|resolved| resolved.events > 0).count()
    }

    /// Notes that the log's event runs changed though its events did not.
    pub(crate) fn outdate(&mut self) {
        self.events = usize::MAX;
    }
}
