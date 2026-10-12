//! Replaying an operation log into a document.

use crate::oplog::{OpKind, OpLog, OpSlice};
use crate::shift::Shifter;
use crate::tracker::Tracker;
use std::ops::Range;
use strider_graph::{Move, Runs, Segment, Walker};
use strider_text::Rewind;

/// The edits that merged zones come to, in the order they are made.
#[derive(Default)]
pub(crate) struct Edits {
    made: Vec<Edit>,
    /// Length of the document so far.
    len: u32,
}

struct Edit {
    pos: u32,
    len: u32,
    /// Where the inserted text is in the log; empty for a removal.
    text: (u32, u32),
}

impl Edits {
    pub(crate) fn insert(&mut self, pos: u32, text: (u32, u32), len: u32) {
        self.len += len;
        self.made.push(Edit { pos, len, text });
    }

    pub(crate) fn remove(&mut self, pos: u32, len: u32) {
        self.len -= len;
        self.made.push(Edit { pos, len, text: (0, 0) });
    }

    /// Where things stand, to come back to with [`undo_to`](Self::undo_to).
    pub(crate) fn mark(&self) -> (usize, u32) {
        (self.made.len(), self.len)
    }

    /// Forgets the edits made since `mark`.
    pub(crate) fn undo_to(&mut self, mark: (usize, u32)) {
        self.made.truncate(mark.0);
        self.len = mark.1;
    }
}

/// What the critical version at the start of a zone keeps about it, once the zone has been
/// through the tracker.
#[derive(Default)]
struct Kept {
    /// How many of the zone's events, counting from its start, `resolved` covers.
    events: u32,
    /// Tracker items, op by op in log order. An insert: the left origin of its first
    /// character, then the right origin all share. A delete: for each stretch of
    /// consecutive targets, the first, then how many, with [`DESCENDING`] if they count down.
    resolved: Vec<u32>,
}

const DESCENDING: u32 = 1 << 31;

/// What a log has worked out in earlier replays, and the working room they used.
#[derive(Default)]
pub(crate) struct Memo {
    plan: Option<Plan>,
    tracker: Tracker,
    walker: Walker,
    shifter: Shifter,
    /// How many edits the zones came to last time: room to set aside for them next time.
    zone_edits: usize,
}

/// The shape of the history, valid while the log has the same number of events.
struct Plan {
    events: usize,
    runs: Runs,
    segments: Vec<Segment>,
    /// For each run, the index of the op containing its first event.
    first_op: Vec<u32>,
    /// For each segment, what the critical version it starts at keeps. Nothing, unless the
    /// segment is a zone that has been through the tracker.
    kept: Vec<Kept>,
}

impl Plan {
    /// Analyses the log. Zones that start from a critical version of the `previous` plan
    /// take over what it kept for them.
    fn new(log: &OpLog, mut previous: Option<Plan>) -> Self {
        let runs = log.graph.runs();
        let segments = runs.segments();
        let first_op = log.ops_containing((0..runs.len()).map(|run| runs.start(run)));
        let kept = segments
            .iter()
            .map(|segment| match (segment, &mut previous) {
                (Segment::Zone(zone), Some(previous)) => previous.take_kept(runs.start(zone.start)),
                _ => Kept::default(),
            })
            .collect();
        Plan { events: log.len(), runs, segments, first_op, kept }
    }

    /// What is kept for the zone that starts at event `start`, if there is one.
    fn take_kept(&mut self, start: u32) -> Kept {
        let runs = &self.runs;
        match self.segments.binary_search_by_key(&start, |segment| runs.start(segment.runs().start)) {
            Ok(at) => std::mem::take(&mut self.kept[at]),
            Err(_) => Kept::default(),
        }
    }

    /// How many zones have been through the tracker.
    fn tracked_zones(&self) -> usize {
        self.kept.iter().filter(|kept| kept.events > 0).count()
    }
}

impl Memo {
    /// How many zones the last checkout could not merge by shifting alone.
    pub(crate) fn tracked_zones(&self) -> usize {
        self.plan.as_ref().map_or(0, Plan::tracked_zones)
    }
}

pub(crate) fn checkout(log: &OpLog, memo: &mut Memo) -> String {
    // The log only ever grows, so the same length means the same events.
    if memo.plan.as_ref().is_none_or(|plan| plan.events != log.len()) {
        memo.plan = Some(Plan::new(log, memo.plan.take()));
    }
    let Memo { plan, tracker, walker, shifter, zone_edits } = memo;
    let plan = plan.as_mut().unwrap();
    let mut kept = std::mem::take(&mut plan.kept);

    // Forwards: merge the zones. The events of a linear stretch are edits already, so all
    // that is needed of them for now is what they do to the document's length.
    let linear_events = |linear: &Range<usize>| plan.runs.start(linear.start)..plan.runs.start(linear.end);
    let mut doc = Edits { made: Vec::with_capacity(*zone_edits), len: 0 };
    let mut edits_before = Vec::with_capacity(plan.segments.len());
    for (segment, kept) in plan.segments.iter().zip(&mut kept) {
        edits_before.push(doc.made.len());
        match segment {
            Segment::Linear(linear) => {
                let (inserted, removed) = log.lengths(plan.first_op[linear.start], linear_events(linear));
                doc.len = doc.len + inserted - removed;
            }
            Segment::Zone(zone) => merge_zone(log, plan, zone, kept, tracker, walker, shifter, &mut doc),
        }
    }

    // Backwards: take every edit back, starting from the final document.
    let mut text = Rewind::new(doc.len);
    let mut edits_end = doc.made.len();
    for (segment, &edits_start) in plan.segments.iter().zip(&edits_before).rev() {
        match segment {
            Segment::Linear(linear) => {
                for op in log.ops(plan.first_op[linear.start], linear_events(linear)).rev() {
                    match op.kind {
                        OpKind::Ins => text.undo_insert(op.pos, log.text(op.text), op.len),
                        OpKind::Del => text.undo_remove(op.pos, op.len),
                    }
                }
            }
            Segment::Zone(_) => {
                for edit in doc.made[edits_start..edits_end].iter().rev() {
                    if edit.text.0 < edit.text.1 {
                        text.undo_insert(edit.pos, log.text(edit.text), edit.len);
                    } else {
                        text.undo_remove(edit.pos, edit.len);
                    }
                }
            }
        }
        edits_end = edits_start;
    }
    plan.kept = kept;
    *zone_edits = doc.made.len();
    text.finish()
}

/// Merges one zone, adding the edits it comes to to `doc`.
#[allow(clippy::too_many_arguments)]
fn merge_zone(log: &OpLog, plan: &Plan, zone: &Range<usize>, kept: &mut Kept, tracker: &mut Tracker, walker: &mut Walker, shifter: &mut Shifter, doc: &mut Edits) {
    // Nothing kept: the zone has not needed the tracker so far. See whether it does now.
    if kept.events == 0 && shifter.merge(log, &plan.runs, &plan.first_op, zone, doc) {
        return;
    }
    let runs = &plan.runs;
    let (start, end) = (runs.start(zone.start), runs.start(zone.end));
    let first_op = plan.first_op[zone.start];
    let goes_before = |a: u32, b: u32| log.inserts_before(start + a, start + b);
    tracker.begin_zone((end - start) as usize);

    // Runs resolved in full by earlier replays are placed by id. Their events stay in the
    // tracker's prepare version, which is where the walk of the rest begins.
    let known_runs = (zone.start..zone.end).take_while(|&run| runs.start(run + 1) - start <= kept.events).count();
    let mut resolved = kept.resolved.iter().copied();
    let mut next = || resolved.next().expect("something kept for every resolved event");
    for op in log.ops(first_op, start..runs.start(zone.start + known_runs)) {
        let lv = op.lv - start;
        match op.kind {
            OpKind::Ins => {
                let (left, right) = (next(), next());
                let pos = tracker.insert_by_id(lv, op.len, left, right, goes_before);
                doc.insert(pos, op.text, op.len);
            }
            OpKind::Del => {
                let mut done = 0;
                while done < op.len {
                    let (first, stretch) = (next(), next());
                    let n = (stretch & !DESCENDING).min(op.len - done);
                    let targets = (0..n).map(|i| if stretch & DESCENDING == 0 { first + i } else { first - i });
                    tracker.delete_by_id(lv + done, targets, |pos, len| doc.remove(pos, len));
                    done += n;
                }
            }
        }
    }
    if known_runs == zone.len() {
        return;
    }

    walker.walk(runs, zone, known_runs, |step| match step {
        Move::Retreat(events) => tracker.retreat(events.start - start..events.end - start),
        Move::Advance(events) => tracker.advance(events.start - start..events.end - start),
        Move::Apply(run) => {
            for op in log.ops(plan.first_op[run], runs.events(run)) {
                walk_op(start, op, tracker, &goes_before, doc);
            }
        }
    });

    // The tracker now knows what every event of the zone resolved to. Keep that.
    kept.events = end - start;
    kept.resolved.clear();
    for op in log.ops(first_op, start..end) {
        let lv = op.lv - start;
        match op.kind {
            OpKind::Ins => kept.resolved.extend(tracker.origins(lv)),
            OpKind::Del => {
                let mut done = 0;
                while done < op.len {
                    let first = tracker.target(lv + done);
                    let stretch = |step: u32| {
                        let follows = |&i: &u32| tracker.target(lv + done + i) == first.wrapping_add(step.wrapping_mul(i));
                        (1..op.len - done).take_while(follows).count() as u32 + 1
                    };
                    let (up, down) = (stretch(1), stretch(1u32.wrapping_neg()));
                    kept.resolved.extend(if up >= down { [first, up] } else { [first, down | DESCENDING] });
                    done += up.max(down);
                }
            }
        }
    }
}

/// Merges one op at the tracker's prepare version.
fn walk_op(start: u32, op: OpSlice, tracker: &mut Tracker, goes_before: &impl Fn(u32, u32) -> bool, doc: &mut Edits) {
    let lv = op.lv - start;
    match op.kind {
        OpKind::Ins => {
            let pos = tracker.insert(lv, op.len, op.pos, goes_before);
            doc.insert(pos, op.text, op.len);
        }
        OpKind::Del => {
            let mut done = 0;
            while done < op.len {
                let (n, pos) = tracker.delete(lv + done, op.pos, op.len - done, op.fwd);
                if let Some(pos) = pos {
                    doc.remove(pos, n);
                }
                done += n;
            }
        }
    }
}
