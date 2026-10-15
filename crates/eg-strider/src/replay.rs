use crate::crdt_records::CrdtRecords;
use crate::delete_targets::DeleteTargets;
use crate::document_state::DocumentState;
use crate::operation_kind::OperationKind;
use crate::operation_run_slice::OperationRunSlice;
use crate::oplog::{ByteRange, OpLog};
use crate::replay_plan::{ReplayPlan, ResolvedOrigins};
use crate::transform::merge_by_transforming;
use crate::transformed_operations::TransformedOperations;
use std::ops::Range;
use strider_graph::{walk, EventIndex, EventRunIndex, HistoryRange, WalkStep};
use strider_text::PieceTable;

pub(crate) const BACKWARD_TARGETS: u32 = 1 << 31;

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
    // Each run of a concurrent range after the base is begun at most once, so that is room
    // enough for the merge order.
    let concurrent_runs = plan.ranges.iter().map(|range| match range {
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

/// Replays one concurrent range with the CRDT records, recording in `transformed` where its
/// operations go.
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
