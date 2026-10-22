use crate::crdt_records::{CrdtRecords, DeleteTargets, RecordId};
use crate::operation::{ByteRange, OperationKind, OperationRunSlice};
use crate::oplog::{OpLog, OperationRunIndex};
use crate::transform::merge_by_transforming;
use std::ops::Range;
use strider_graph::{walk, EventIndex, EventRunIndex, HistoryRange, WalkStep};
use strider_text::PieceTable;

/// The paper's transformed operations for the concurrent ranges, stored as their positions in the
/// effect version; the rest of each is its operation run.
#[derive(Default)]
pub(crate) struct TransformedOperations {
    /// Ranges of event runs, in merge order. Within a range, an operation run spanning several
    /// event runs is one operation.
    merge_order: Vec<Range<EventRunIndex>>,
    /// Per operation run of those ranges. An insert: the index it went in at. A delete: the index
    /// and length of each piece it removed, then the number of pieces.
    positions: Vec<u32>,
    /// Pieces the current delete has removed so far.
    removed_pieces: u32,
    /// Length of the effect version so far.
    len: u32,
    /// The first run whose events the base does not hold; earlier runs' operations are not
    /// recorded.
    first_new_run: EventRunIndex,
    /// Whether the current range of runs is of those earlier runs.
    in_base: bool,
}

impl TransformedOperations {
    /// Starts a range of runs, all or none of which the base holds.
    pub(crate) fn begin(&mut self, run_range: Range<EventRunIndex>) {
        self.in_base = run_range.end <= self.first_new_run;
        debug_assert!(self.in_base || run_range.start >= self.first_new_run, "runs with events on both sides of the base");
        if !self.in_base {
            self.merge_order.push(run_range);
        }
    }

    /// Records that an insert of `len` characters went in at `pos`.
    pub(crate) fn insert(&mut self, pos: u32, len: u32) {
        if !self.in_base {
            self.len += len;
            self.positions.push(pos);
        }
    }

    /// Records that the current delete removed the `len` characters at `pos`.
    pub(crate) fn remove(&mut self, pos: u32, len: u32) {
        if !self.in_base {
            self.len -= len;
            self.positions.extend([pos, len]);
            self.removed_pieces += 1;
        }
    }

    /// Ends the current delete.
    pub(crate) fn end_delete(&mut self) {
        if !self.in_base {
            self.positions.push(std::mem::take(&mut self.removed_pieces));
        }
    }

    /// Marks where things stand, for [`undo_to`](Self::undo_to).
    pub(crate) fn mark(&self) -> (usize, usize, u32) {
        (self.merge_order.len(), self.positions.len(), self.len)
    }

    /// Forgets what was recorded since `mark`.
    pub(crate) fn undo_to(&mut self, mark: (usize, usize, u32)) {
        self.merge_order.truncate(mark.0);
        self.positions.truncate(mark.1);
        self.len = mark.2;
    }
}

/// The CRDT records' result for a concurrent range, cached at its critical version so that a later
/// replay places its events without walking them.
#[derive(Default)]
struct ResolvedOrigins {
    /// How many of the range's events, from its start, `resolved` covers.
    events: u32,
    /// Records, operation run by operation run in log order. An insert: its first character's left
    /// origin, then the right origin all its characters share. A delete: per group of consecutive
    /// targets, the first and the count, with [`BACKWARD_TARGETS`] if they count down.
    resolved: Vec<RecordId>,
}

/// Flags a count of delete targets whose records count down.
const BACKWARD_TARGETS: u32 = 1 << 31;

/// The document as of the first `events` events.
#[derive(Default)]
pub(crate) struct DocumentState {
    pub events: EventIndex,
    /// The length of `text` in characters.
    pub chars: u32,
    pub text: String,
}

/// What replays of a log keep between checkouts while its events stay the same: its ranges, and the
/// CRDT records' cached results.
#[derive(Default)]
pub(crate) struct ReplayPlan {
    /// The number of events the plan was made for.
    events: usize,
    /// The first event of each range.
    range_starts: Vec<EventIndex>,
    ranges: Vec<HistoryRange>,
    /// Per event run, the operation run holding its first event; then the number of operation runs.
    first_op_of_run: Vec<OperationRunIndex>,
    /// Per range, the cached CRDT result: empty for linear ranges and those merged by transforming.
    resolved: Vec<ResolvedOrigins>,
    /// How many positions the concurrent ranges took last time, to reserve next time.
    room: usize,
}

impl ReplayPlan {
    /// Analyses the log, taking over what the `previous` plan resolved for concurrent ranges that
    /// start at the same event. Runs before `changed_from` are as they were when `previous` was
    /// made, unless it was outdated; only what follows them is analysed again.
    fn new(log: &OpLog, previous: Option<ReplayPlan>, changed_from: EventRunIndex) -> Self {
        let runs = log.graph.runs();
        let Some(previous) = previous else {
            return ReplayPlan::from_run(log, ReplayPlan::default(), 0, 0);
        };
        // The range holding the last run that did not change starts at a critical version that
        // did not either; everything from it on is cut again.
        let unchanged = (changed_from as usize).min(previous.first_op_of_run.len() - 1);
        if previous.events == usize::MAX || unchanged == 0 || previous.ranges.is_empty() {
            return ReplayPlan::from_run(log, previous, 0, 0);
        }
        let kept = previous.ranges.partition_point(|range| range.runs().end < unchanged as EventRunIndex);
        let from = previous.ranges[kept].runs().start;
        debug_assert!(runs.start(from) == previous.range_starts[kept], "a range kept that changed");
        let plan = ReplayPlan::from_run(log, previous, kept, from);
        if cfg!(debug_assertions) {
            let fresh = ReplayPlan::from_run(log, ReplayPlan::default(), 0, 0);
            debug_assert_eq!(plan.ranges, fresh.ranges, "ranges cut again from run {from}");
            debug_assert_eq!(plan.range_starts, fresh.range_starts);
            debug_assert_eq!(plan.first_op_of_run, fresh.first_op_of_run, "ops found again from run {from}");
        }
        plan
    }

    /// The plan with `previous`'s first `kept` ranges, which end where run `from` starts, and the
    /// history from run `from` cut again.
    fn from_run(log: &OpLog, mut previous: ReplayPlan, kept: usize, from: EventRunIndex) -> Self {
        let runs = log.graph.runs();
        let (old_starts, old_resolved) = (previous.range_starts.split_off(kept), previous.resolved.split_off(kept));
        let (mut ranges, mut range_starts, mut resolved) = (previous.ranges, previous.range_starts, previous.resolved);
        ranges.truncate(kept);
        let mut more = log.graph.split_at_critical_versions_from(from);
        // A linear range is as long as it can be: the one kept last and the first cut again may
        // be one.
        if let (Some(HistoryRange::Linear(last)), Some(HistoryRange::Linear(first))) = (ranges.last_mut(), more.first()) {
            last.end = first.end;
            more.remove(0);
        }
        let mut old = (old_starts, old_resolved);
        for range in &more {
            let start = runs.start(range.runs().start);
            range_starts.push(start);
            resolved.push(match (range, old.0.binary_search(&start)) {
                (HistoryRange::Concurrent(_), Ok(at)) => std::mem::take(&mut old.1[at]),
                _ => ResolvedOrigins::default(),
            });
        }
        ranges.extend(more);
        // Runs before `from` start in the same ops as before.
        let mut first_op_of_run = previous.first_op_of_run;
        first_op_of_run.truncate(from as usize);
        let from_op = first_op_of_run.last().copied().unwrap_or(0);
        first_op_of_run.extend(log.ops_containing(from_op, (from..=runs.len()).map(|run| runs.start(run))));
        ReplayPlan { events: log.len(), range_starts, ranges, first_op_of_run, resolved, room: previous.room }
    }

    /// Iterates over the operations of a range of runs, cutting any that run past either end.
    #[inline]
    fn ops<'a>(&self, log: &'a OpLog, run_range: &Range<EventRunIndex>) -> impl DoubleEndedIterator<Item = OperationRunSlice> + 'a {
        self.ops_from(log, run_range, log.graph.runs().start(run_range.start))
    }

    /// Iterates over the operations of a range of runs from event `from`, which is in it.
    #[inline]
    fn ops_from<'a>(&self, log: &'a OpLog, run_range: &Range<EventRunIndex>, from: EventIndex) -> impl DoubleEndedIterator<Item = OperationRunSlice> + 'a {
        log.ops(self.op_at(log, run_range, from)..self.first_op_of_run[run_range.end as usize], from..log.graph.runs().start(run_range.end))
    }

    /// The operation run holding event `from` of `run_range`.
    fn op_at(&self, log: &OpLog, run_range: &Range<EventRunIndex>, from: EventIndex) -> OperationRunIndex {
        if from == log.graph.runs().start(run_range.start) {
            self.first_op_of_run[run_range.start as usize]
        } else {
            log.op_containing(from)
        }
    }

    /// How many concurrent ranges have a cached CRDT result.
    pub(crate) fn ranges_using_crdt_records(&self) -> usize {
        self.resolved.iter().filter(|resolved| resolved.events > 0).count()
    }

    /// Notes that the log's event runs changed though its events did not.
    pub(crate) fn outdate(&mut self) {
        self.events = usize::MAX;
    }
}

/// Replays the events after `base` onto it, returning the text and its length in characters.
///
/// Linear ranges apply as recorded; a concurrent range is merged by transforming if it can be, else
/// with the CRDT records, whose result `plan` caches. The text is then built by undoing every
/// operation last to first. If `base` ends inside a concurrent range, an event run and an operation
/// run must start there.
pub(crate) fn replay(log: &OpLog, plan: &mut Option<ReplayPlan>, base: &DocumentState, changed_from: EventRunIndex) -> (String, u32) {
    // The log only ever grows, so the same length means the same events.
    if plan.as_ref().is_none_or(|plan| plan.events != log.len()) {
        *plan = Some(ReplayPlan::new(log, plan.take(), changed_from));
    }
    let plan = plan.as_mut().expect("a plan was just made");
    let mut resolved = std::mem::take(&mut plan.resolved);

    // Forwards: merge the concurrent ranges. The events of a linear range are operations already,
    // so all that is needed of them for now is what they do to the document's length.
    let runs = log.graph.runs();
    let events = |run_range: &Range<EventRunIndex>| runs.start(run_range.start)..runs.start(run_range.end);
    debug_assert!((log.graph.first_held_event()..log.graph.len()).contains(&base.events), "a base the log does not reach back to");
    let first_new_run = runs.containing(base.events);
    // The ranges before the one holding the base have nothing to replay.
    let first_range = plan.range_starts.partition_point(|&start| start <= base.events).saturating_sub(1);
    // Each run of a concurrent range after the base is begun at most once, so that is room
    // enough for the merge order.
    let concurrent_runs = plan.ranges[first_range..].iter().map(|range| match range {
        HistoryRange::Concurrent(concurrent) => concurrent.end.saturating_sub(concurrent.start.max(first_new_run)) as usize,
        HistoryRange::Linear(_) => 0,
    });
    let mut transformed = TransformedOperations {
        merge_order: Vec::with_capacity(concurrent_runs.sum()),
        positions: Vec::with_capacity(plan.room),
        removed_pieces: 0,
        len: base.chars,
        first_new_run,
        in_base: false,
    };
    // Space that transforming reuses from one concurrent range to the next, since a merge can
    // transform thousands.
    let (mut versions, mut stable, mut edited, mut spare) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    // The ranges that have events after the base, and for each the first of those.
    let after = |range: &HistoryRange| Some(events(range.runs())).filter(|events| events.end > base.events).map(|events| events.start.max(base.events));
    for (range, resolved) in plan.ranges.iter().zip(&mut resolved).skip(first_range) {
        let Some(from) = after(range) else { continue };
        match range {
            HistoryRange::Linear(linear) => {
                // A history with no concurrency is one range, and the log keeps its count.
                let whole = plan.ranges.len() == 1 && from == runs.start(0);
                let (inserted, removed) = if whole { log.total_lengths() } else { log.lengths(plan.op_at(log, linear, from), from..runs.start(linear.end)) };
                transformed.len = transformed.len + inserted - removed;
            }
            HistoryRange::Concurrent(concurrent) => {
                debug_assert_eq!(from, runs.start(first_new_run.max(concurrent.start)), "a base partway through a run of a concurrent range");
                // Nothing resolved: the range has not needed the CRDT records so far. See whether
                // it does now.
                let merged = resolved.events == 0
                    && merge_by_transforming(
                        log,
                        runs,
                        &plan.first_op_of_run,
                        concurrent,
                        &mut transformed,
                        &mut versions,
                        &mut stable,
                        &mut edited,
                        &mut spare,
                    );
                if !merged {
                    replay_concurrent_range(log, plan, concurrent, resolved, &mut transformed)
                }
            }
        }
    }
    // The text is built without the room that merging took.
    drop((versions, stable, edited, spare));
    plan.resolved = resolved;
    plan.room = transformed.positions.len();

    // Backwards: take every operation back, starting from the final document.
    let mut text = PieceTable::new(transformed.len, log.inserted_text());
    let bytes = |(start, end): ByteRange| start as usize..end as usize;
    let mut merge_order = transformed.merge_order.iter().rev().peekable();
    let mut positions = transformed.positions.iter().rev().copied();
    let mut position = || positions.next().expect("a position for every operation of a concurrent range");
    let mut trimming_deletes: Vec<OperationRunSlice> = Vec::new();
    for range in plan.ranges.iter().rev() {
        let Some(from) = after(range) else { break };
        match range {
            HistoryRange::Linear(linear) => {
                for op in plan.ops_from(log, linear, from).rev() {
                    // A delete that takes back the end of the insert before it is met first.
                    // The two together are a shorter insert, so it only has to be counted.
                    if op.deletes_end_of_previous_insert {
                        trimming_deletes.push(op);
                        continue;
                    }
                    let cut = trimming_deletes.iter().map(|delete| delete.len).sum::<u32>();
                    let folds = cut > 0
                        && op.kind == OperationKind::Insert
                        && cut <= op.len
                        && trimming_deletes.last().is_some_and(|first| first.first_event == op.first_event + op.len);
                    if cut > 0 && !folds {
                        // Not enough of the insert is here: this range begins partway through
                        // it.
                        trimming_deletes.drain(..).for_each(|delete| text.undo_remove(delete.pos, delete.len));
                    }
                    match op.kind {
                        OperationKind::Insert if folds => {
                            text.undo_insert(op.pos, bytes(log.trimmed(op.text, op.len, cut)), op.len - cut);
                            trimming_deletes.clear();
                        }
                        OperationKind::Insert => text.undo_insert(op.pos, bytes(op.text), op.len),
                        OperationKind::Delete => text.undo_remove(op.pos, op.len),
                    }
                }
                trimming_deletes.drain(..).for_each(|delete| text.undo_remove(delete.pos, delete.len));
            }
            HistoryRange::Concurrent(concurrent) => {
                while let Some(run_range) = merge_order.next_if(|run_range| run_range.start >= concurrent.start) {
                    for op in plan.ops(log, run_range).rev() {
                        match op.kind {
                            OperationKind::Insert => text.undo_insert(position(), bytes(op.text), op.len),
                            OperationKind::Delete => {
                                for _ in 0..position() {
                                    let (len, pos) = (position(), position());
                                    text.undo_remove(pos, len);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    // What is left is the document at the base, whose text is already known.
    text.undo_rest(&base.text);
    (text.into_string(), transformed.len)
}

/// Merges a concurrent range with the CRDT records, recording in `transformed` where its operations
/// go.
fn replay_concurrent_range(
    log: &OpLog,
    plan: &ReplayPlan,
    range: &Range<EventRunIndex>,
    resolved: &mut ResolvedOrigins,
    transformed: &mut TransformedOperations,
) {
    let runs = log.graph.runs();
    let (start, end) = (runs.start(range.start), runs.start(range.end));
    let goes_before = |a: EventIndex, b: EventIndex| log.inserts_before(start + a, start + b);
    // The range's runs that the document already holds, which are its first. The document was
    // no longer when the range began than it is now plus what those events removed.
    let held_until = transformed.first_new_run.clamp(range.start, range.end);
    let mut records = CrdtRecords::new((end - start) as usize, transformed.len + (runs.start(held_until) - start));

    // Runs resolved in full by earlier replays are placed by record. Their events stay in the
    // records' prepare version, which is where the walk of the rest begins.
    let known_runs = (range.start..range.end).take_while(|&run| runs.start(run + 1) - start <= resolved.events).count();
    let known = range.start..range.start + known_runs as EventRunIndex;
    let mut origins = resolved.resolved.iter().copied();
    let mut next = || origins.next().expect("something resolved for every resolved event");
    let parted = held_until.min(known.end);
    for run_range in [known.start..parted, parted..known.end] {
        if run_range.is_empty() {
            continue;
        }
        transformed.begin(run_range.clone());
        for op in plan.ops(log, &run_range) {
            let event = op.first_event - start;
            match op.kind {
                OperationKind::Insert => {
                    let (left, right) = (next(), next());
                    let pos = records.insert_with_known_origins(event, op.len, left, right, goes_before);
                    transformed.insert(pos, op.len);
                }
                OperationKind::Delete => {
                    let mut done = 0;
                    while done < op.len {
                        let (first, count) = (next(), next());
                        let (n, backward) = ((count & !BACKWARD_TARGETS).min(op.len - done), count & BACKWARD_TARGETS != 0);
                        let record = if backward { first + 1 - n } else { first };
                        records.delete_with_known_targets(DeleteTargets { first_event: event + done, len: n, record, backward }, |pos, len| {
                            transformed.remove(pos, len)
                        });
                        done += n;
                    }
                    transformed.end_delete();
                }
            }
        }
    }
    if known_runs == range.len() {
        return;
    }

    // What the document holds is gone through before anything else, so that where the rest goes
    // is a position in that document.
    walk(runs, range, known_runs, (held_until - range.start) as usize, |step| match step {
        WalkStep::Retreat(run_range) => {
            for op in plan.ops(log, &run_range) {
                records.retreat(op.kind, op.first_event - start..op.first_event - start + op.len);
            }
        }
        WalkStep::Advance(run_range) => {
            for op in plan.ops(log, &run_range) {
                records.advance(op.kind, op.first_event - start..op.first_event - start + op.len);
            }
        }
        WalkStep::Apply(run) => {
            transformed.begin(run..run + 1);
            for op in plan.ops(log, &(run..run + 1)) {
                apply(start, op, &mut records, &goes_before, transformed);
            }
        }
    });

    // The records now know what every event of the range resolved to. Keep that.
    resolved.events = end - start;
    resolved.resolved.clear();
    for op in plan.ops(log, range) {
        let event = op.first_event - start;
        match op.kind {
            OperationKind::Insert => resolved.resolved.extend(records.origins_of(event)),
            OperationKind::Delete => {
                let mut done = 0;
                while done < op.len {
                    let targets = records.targets_of(event + done, event + op.len);
                    resolved.resolved.extend([targets.first(), targets.len | if targets.backward { BACKWARD_TARGETS } else { 0 }]);
                    done += targets.len;
                }
            }
        }
    }
    resolved.resolved.shrink_to_fit();
}

/// Applies an operation run at the CRDT records' prepare version.
fn apply(
    start: EventIndex,
    op: OperationRunSlice,
    records: &mut CrdtRecords,
    goes_before: &impl Fn(EventIndex, EventIndex) -> bool,
    transformed: &mut TransformedOperations,
) {
    let event = op.first_event - start;
    match op.kind {
        OperationKind::Insert => {
            let pos = records.insert(event, op.len, op.pos, goes_before);
            transformed.insert(pos, op.len);
        }
        OperationKind::Delete => {
            records.delete(event, op.pos, op.len, op.backward, |pos, len| transformed.remove(pos, len));
            transformed.end_delete();
        }
    }
}
