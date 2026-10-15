use crate::crdt_records::{CrdtRecords, DeleteTargets, RecordId};
use crate::oplog::{ByteRange, OpLog, OperationKind, OperationRunIndex, OperationRunSlice};
use crate::transform::Transform;
use std::ops::Range;
use strider_graph::{walk, EventIndex, EventRunIndex, HistoryRange, WalkStep};
use strider_text::PieceTable;

/// Where merging put each operation of the concurrent ranges: the paper's transformed
/// operations, as positions in the effect version. The rest of each operation is the operation
/// run itself.
#[derive(Default)]
pub(crate) struct TransformedOperations {
    /// Ranges of event runs, in merge order. An operation run that goes from one event run into
    /// the next is one operation run.
    merge_order: Vec<Range<EventRunIndex>>,
    /// Per operation run of those ranges. An insert: the index it went in at. A delete: the index
    /// and length of each piece it removed, then the number of pieces.
    positions: Vec<u32>,
    /// Pieces the current delete has removed so far.
    removed_pieces: u32,
    /// Length of the document so far.
    len: u32,
    /// The first run whose events the document does not hold. Operations of earlier runs are not
    /// recorded.
    first_new_run: EventRunIndex,
    /// Whether the current range of runs is of those earlier runs.
    in_base: bool,
}

impl TransformedOperations {
    /// Starts on a range of runs, which the document holds all of or none of.
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

    /// Ends a delete once all it removed is recorded.
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

/// What a concurrent range's critical version keeps once the range has been through the CRDT
/// records, so that a later replay can place its events by record.
#[derive(Default)]
struct ResolvedOrigins {
    /// How many of the range's events, from its start, `resolved` covers.
    events: u32,
    /// Records, operation run by operation run in log order. An insert: its first character's
    /// left origin, then the right origin all its characters share. A delete: per group of
    /// consecutive targets, the first and the count, with [`BACKWARD_TARGETS`] if they count down.
    resolved: Vec<RecordId>,
}

const BACKWARD_TARGETS: u32 = 1 << 31;

/// The document state with the first `events` events merged, which a replay of the rest starts
/// from.
#[derive(Default)]
pub(crate) struct DocumentState {
    pub events: EventIndex,
    /// The length of `text` in characters.
    pub chars: u32,
    pub text: String,
}

/// The history's shape, and what earlier replays of a log worked out, valid while the log has
/// the same number of events.
pub(crate) struct ReplayPlan {
    events: usize,
    /// The first event of each range.
    range_starts: Vec<EventIndex>,
    ranges: Vec<HistoryRange>,
    /// Per event run, the operation run holding its first event; then the number of operation
    /// runs.
    first_op_of_run: Vec<OperationRunIndex>,
    /// Per range, what its starting critical version keeps: nothing unless it is a concurrent
    /// range that went through the CRDT records.
    resolved: Vec<ResolvedOrigins>,
    /// How many ranges of runs and positions the concurrent ranges took last time, to reserve
    /// next time.
    room: (usize, usize),
}

impl ReplayPlan {
    /// Analyses the log, taking over what the `previous` plan resolved for concurrent ranges that
    /// start at the same event.
    fn new(log: &OpLog, mut previous: Option<ReplayPlan>) -> Self {
        let room = previous.as_ref().map_or((0, 0), |previous| previous.room);
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
    fn take_resolved(&mut self, start: EventIndex) -> ResolvedOrigins {
        match self.range_starts.binary_search(&start) {
            Ok(at) => std::mem::take(&mut self.resolved[at]),
            Err(_) => ResolvedOrigins::default(),
        }
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

    /// Returns the operation run holding event `from` of a range of runs.
    fn op_at(&self, log: &OpLog, run_range: &Range<EventRunIndex>, from: EventIndex) -> OperationRunIndex {
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

/// Replays the log's events after `base` into the document state `base` holds, reusing and
/// updating the `plan` that earlier replays made. Returns the text and its length in characters.
///
/// Linear ranges apply as recorded. A concurrent range is merged by transforming if it can be,
/// else with the CRDT records, whose results are cached at the range's critical version. Merging
/// only works out where each operation goes; the text is then built by taking the operations
/// back, last to first. If `base` ends inside a concurrent range, an event run and an operation
/// run must start there: the range's events that `base` holds are walked first and not recorded.
pub(crate) fn replay(log: &OpLog, plan: &mut Option<ReplayPlan>, base: &DocumentState) -> (String, u32) {
    // The log only ever grows, so the same length means the same events.
    if plan.as_ref().is_none_or(|plan| plan.events != log.len()) {
        *plan = Some(ReplayPlan::new(log, plan.take()));
    }
    let plan = plan.as_mut().expect("a plan was just made");
    let mut resolved = std::mem::take(&mut plan.resolved);

    // Forwards: merge the concurrent ranges. The events of a linear range are operations already,
    // so all that is needed of them for now is what they do to the document's length.
    let runs = log.graph.runs();
    let events = |run_range: &Range<EventRunIndex>| runs.start(run_range.start)..runs.start(run_range.end);
    debug_assert!((log.graph.first_held_event()..log.graph.len()).contains(&base.events), "a base the log does not reach back to");
    let first_new_run = runs.containing(base.events);
    let mut transformed = TransformedOperations {
        merge_order: Vec::with_capacity(plan.room.0),
        positions: Vec::with_capacity(plan.room.1),
        removed_pieces: 0,
        len: base.chars,
        first_new_run,
        in_base: false,
    };
    let mut transform = Transform::default();
    // The ranges that have events after the base, and for each the first of those.
    let after = |range: &HistoryRange| Some(events(range.runs())).filter(|events| events.end > base.events).map(|events| events.start.max(base.events));
    for (range, resolved) in plan.ranges.iter().zip(&mut resolved) {
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
                replay_concurrent_range(log, plan, concurrent, resolved, &mut transform, &mut transformed)
            }
        }
    }
    // The text is built without the room that merging took.
    drop(transform);
    plan.resolved = resolved;
    plan.room = (transformed.merge_order.len(), transformed.positions.len());

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

/// Replays one concurrent range, recording in `transformed` where its operations go.
fn replay_concurrent_range(
    log: &OpLog,
    plan: &ReplayPlan,
    range: &Range<EventRunIndex>,
    resolved: &mut ResolvedOrigins,
    transform: &mut Transform,
    transformed: &mut TransformedOperations,
) {
    // Nothing resolved: the range has not needed the CRDT records so far. See whether it does now.
    if resolved.events == 0 && transform.try_merge(log, log.graph.runs(), &plan.first_op_of_run, range, transformed) {
        return;
    }
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

/// Applies one operation run at the CRDT records' prepare version.
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
