//! Replays an operation log into the document's text. The history is cut at its critical
//! versions. A stretch with no concurrency is applied as recorded. A region is first tried by
//! offset arithmetic ([`shift`](crate::shift)) and otherwise walked with the tracker; what
//! the tracker resolves is memoised at the region's critical version, since it never changes,
//! so later replays place those events directly. Merging only works out where each op of a
//! region goes; the edits are then taken back from last to first to assemble the text.

use crate::oplog::{OpKind, OpLog, OpSlice};
use crate::shift::Shifter;
use crate::tracker::Tracker;
use std::ops::Range;
use strider_graph::{Move, Segment, Walker};
use strider_text::Rewind;

/// Where merging put each op of the regions. The rest of each edit is the op itself, in the
/// log.
#[derive(Default)]
pub(crate) struct Placed {
    /// The stretches of blocks, in the order they were merged. The ops of a stretch are taken
    /// together, so one that runs on from a block into the next is a single op.
    order: Vec<Range<u32>>,
    /// For each op of those stretches in turn. An insert: the index it went in at. A delete:
    /// the index and length of each run of characters it removed, then how many runs.
    at: Vec<u32>,
    /// How many runs the delete being recorded has removed so far.
    runs: u32,
    /// Length of the document so far.
    len: u32,
}

impl Placed {
    /// Starts on the ops of a stretch of blocks.
    pub(crate) fn begin(&mut self, stretch: Range<usize>) {
        self.order.push(stretch.start as u32..stretch.end as u32);
    }

    /// Records that an insert of `len` characters went in at `pos`.
    pub(crate) fn insert(&mut self, pos: u32, len: u32) {
        self.len += len;
        self.at.push(pos);
    }

    /// Records that the delete being merged removed the `len` characters at `pos`.
    pub(crate) fn remove(&mut self, pos: u32, len: u32) {
        self.len -= len;
        self.at.extend([pos, len]);
        self.runs += 1;
    }

    /// Ends a delete, once everything it removed is recorded.
    pub(crate) fn end_delete(&mut self) {
        self.at.push(std::mem::take(&mut self.runs));
    }

    /// Notes where things stand, to come back to with [`undo_to`](Self::undo_to).
    pub(crate) fn mark(&self) -> (usize, usize, u32) {
        (self.order.len(), self.at.len(), self.len)
    }

    /// Forgets what was recorded since `mark`.
    pub(crate) fn undo_to(&mut self, mark: (usize, usize, u32)) {
        self.order.truncate(mark.0);
        self.at.truncate(mark.1);
        self.len = mark.2;
    }
}

/// What the critical version at the start of a region keeps about it, once the region has been
/// through the tracker.
#[derive(Default)]
struct Kept {
    /// How many of the region's events, counting from its start, `resolved` covers.
    events: u32,
    /// Tracker items, op by op in log order. An insert: the left origin of its first
    /// character, then the right origin all share. A delete: for each stretch of
    /// consecutive targets, the first, then how many, with [`DESCENDING`] if they count down.
    resolved: Vec<u32>,
}

const DESCENDING: u32 = 1 << 31;

/// What a log has worked out in earlier replays.
#[derive(Default)]
pub(crate) struct Memo {
    plan: Option<Plan>,
    /// How many stretches and places the regions came to last time: room to set aside for
    /// them next time.
    room: (usize, usize),
}

/// Working room for merging regions, reused from one region to the next.
#[derive(Default)]
struct Workspace {
    tracker: Tracker,
    walker: Walker,
    shifter: Shifter,
}

/// The shape of the history, valid while the log has the same number of events.
struct Plan {
    events: usize,
    /// The first event of each segment.
    starts: Vec<u32>,
    segments: Vec<Segment>,
    /// For each block, the index of the op containing its first event; then the number of ops.
    first_op: Vec<u32>,
    /// For each segment, what the critical version it starts at keeps. Nothing, unless the
    /// segment is a region that has been through the tracker.
    kept: Vec<Kept>,
}

impl Plan {
    /// Analyses the log. Regions that start from a critical version of the `previous` plan
    /// take over what it kept for them.
    fn new(log: &OpLog, mut previous: Option<Plan>) -> Self {
        let blocks = log.graph.blocks();
        let segments = log.graph.segments();
        let starts = segments.iter().map(|segment| blocks.start(segment.blocks().start)).collect();
        let first_op = log.ops_containing((0..=blocks.len()).map(|block| blocks.start(block)));
        let kept = segments
            .iter()
            .map(|segment| match (segment, &mut previous) {
                (Segment::Region(region), Some(previous)) => previous.take_kept(blocks.start(region.start)),
                _ => Kept::default(),
            })
            .collect();
        Plan { events: log.len(), starts, segments, first_op, kept }
    }

    /// Takes what is kept for the region that starts at event `start`, if there is one.
    fn take_kept(&mut self, start: u32) -> Kept {
        match self.starts.binary_search(&start) {
            Ok(at) => std::mem::take(&mut self.kept[at]),
            Err(_) => Kept::default(),
        }
    }

    /// Iterates over the ops of a stretch of blocks. An op that runs past either end is cut
    /// there.
    #[inline]
    fn ops<'a>(&self, log: &'a OpLog, stretch: &Range<usize>) -> impl DoubleEndedIterator<Item = OpSlice> + 'a {
        let blocks = log.graph.blocks();
        log.ops(self.first_op[stretch.start]..self.first_op[stretch.end], blocks.start(stretch.start)..blocks.start(stretch.end))
    }

    /// Counts the regions that have been through the tracker.
    fn tracked_regions(&self) -> usize {
        self.kept.iter().filter(|kept| kept.events > 0).count()
    }
}

impl Memo {
    /// Returns how many regions the last checkout could not merge by shifting alone.
    pub(crate) fn tracked_regions(&self) -> usize {
        self.plan.as_ref().map_or(0, Plan::tracked_regions)
    }
}

/// Merges every event in the log into the document's text, reusing and updating `memo`.
pub(crate) fn checkout(log: &OpLog, memo: &mut Memo) -> String {
    // The log only ever grows, so the same length means the same events.
    if memo.plan.as_ref().is_none_or(|plan| plan.events != log.len()) {
        memo.plan = Some(Plan::new(log, memo.plan.take()));
    }
    let plan = memo.plan.as_mut().expect("a plan was just made");
    let mut kept = std::mem::take(&mut plan.kept);

    // Forwards: merge the regions. The events of a linear stretch are edits already, so all
    // that is needed of them for now is what they do to the document's length.
    let blocks = log.graph.blocks();
    let events = |stretch: &Range<usize>| blocks.start(stretch.start)..blocks.start(stretch.end);
    let mut doc = Placed { order: Vec::with_capacity(memo.room.0), at: Vec::with_capacity(memo.room.1), runs: 0, len: 0 };
    let mut workspace = Workspace::default();
    for (segment, kept) in plan.segments.iter().zip(&mut kept) {
        match segment {
            Segment::Linear(linear) => {
                let (inserted, removed) = log.lengths(plan.first_op[linear.start], events(linear));
                doc.len = doc.len + inserted - removed;
            }
            Segment::Region(region) => merge_region(log, plan, region, kept, &mut workspace, &mut doc),
        }
    }
    // The text is built without the room that merging took.
    drop(workspace);
    plan.kept = kept;
    memo.room = (doc.order.len(), doc.at.len());

    // Backwards: take every edit back, starting from the final document.
    let mut text = Rewind::new(doc.len);
    let mut order = doc.order.iter().rev().peekable();
    let mut places = doc.at.iter().rev().copied();
    let mut place = || places.next().expect("a place for every op of a region");
    for segment in plan.segments.iter().rev() {
        match segment {
            Segment::Linear(linear) => {
                for op in plan.ops(log, linear).rev() {
                    match op.kind {
                        OpKind::Ins => text.undo_insert(op.pos, log.text(op.text), op.len),
                        OpKind::Del => text.undo_remove(op.pos, op.len),
                    }
                }
            }
            Segment::Region(region) => {
                while let Some(stretch) = order.next_if(|stretch| stretch.start as usize >= region.start) {
                    for op in plan.ops(log, &(stretch.start as usize..stretch.end as usize)).rev() {
                        match op.kind {
                            OpKind::Ins => text.undo_insert(place(), log.text(op.text), op.len),
                            OpKind::Del => {
                                for _ in 0..place() {
                                    let (len, pos) = (place(), place());
                                    text.undo_remove(pos, len);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    text.finish()
}

/// Merges one region, recording in `doc` where its ops go.
fn merge_region(log: &OpLog, plan: &Plan, region: &Range<usize>, kept: &mut Kept, workspace: &mut Workspace, doc: &mut Placed) {
    let Workspace { tracker, walker, shifter } = workspace;
    // Nothing kept: the region has not needed the tracker so far. See whether it does now.
    if kept.events == 0 && shifter.merge(log, log.graph.blocks(), &plan.first_op, region, doc) {
        return;
    }
    let blocks = log.graph.blocks();
    let (start, end) = (blocks.start(region.start), blocks.start(region.end));
    let goes_before = |a: u32, b: u32| log.inserts_before(start + a, start + b);
    tracker.begin_region((end - start) as usize);

    // Blocks resolved in full by earlier replays are placed by id. Their events stay in the
    // tracker's prepare version, which is where the walk of the rest begins.
    let known_blocks = (region.start..region.end).take_while(|&block| blocks.start(block + 1) - start <= kept.events).count();
    let mut resolved = kept.resolved.iter().copied();
    let mut next = || resolved.next().expect("something kept for every resolved event");
    if known_blocks > 0 {
        doc.begin(region.start..region.start + known_blocks);
    }
    for op in plan.ops(log, &(region.start..region.start + known_blocks)) {
        let lv = op.lv - start;
        match op.kind {
            OpKind::Ins => {
                let (left, right) = (next(), next());
                let pos = tracker.insert_by_id(lv, op.len, left, right, goes_before);
                doc.insert(pos, op.len);
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
                doc.end_delete();
            }
        }
    }
    if known_blocks == region.len() {
        return;
    }

    walker.walk(blocks, region, known_blocks, |step| match step {
        Move::Retreat(stretch) => {
            for op in plan.ops(log, &stretch) {
                tracker.retreat(op.kind, op.lv - start..op.lv - start + op.len);
            }
        }
        Move::Advance(stretch) => {
            for op in plan.ops(log, &stretch) {
                tracker.advance(op.kind, op.lv - start..op.lv - start + op.len);
            }
        }
        Move::Apply(block) => {
            doc.begin(block..block + 1);
            for op in plan.ops(log, &(block..block + 1)) {
                walk_op(start, op, tracker, &goes_before, doc);
            }
        }
    });

    // The tracker now knows what every event of the region resolved to. Keep that.
    kept.events = end - start;
    kept.resolved.clear();
    for op in plan.ops(log, region) {
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
    kept.resolved.shrink_to_fit();
}

/// Merges one op at the tracker's prepare version.
fn walk_op(start: u32, op: OpSlice, tracker: &mut Tracker, goes_before: &impl Fn(u32, u32) -> bool, doc: &mut Placed) {
    let lv = op.lv - start;
    match op.kind {
        OpKind::Ins => {
            let pos = tracker.insert(lv, op.len, op.pos, goes_before);
            doc.insert(pos, op.len);
        }
        OpKind::Del => {
            tracker.delete(lv, op.pos, op.len, op.fwd, |pos, len| doc.remove(pos, len));
            doc.end_delete();
        }
    }
}
