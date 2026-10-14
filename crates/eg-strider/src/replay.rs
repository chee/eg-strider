//! Replays an operation log into the document's text. The history is cut at its critical
//! versions. A stretch with no concurrency is applied as recorded. A region is first tried by
//! offset arithmetic ([`shift`](crate::shift)) and otherwise walked with the tracker; what
//! the tracker resolves is memoised at the region's critical version, since it never changes,
//! so later replays place those events directly. Merging only works out where each op of a
//! region goes; the edits are then taken back from last to first to assemble the text.
//!
//! A replay starts from a document that already holds the first so many events. Those may
//! end inside a region, when a later event was made without some of them. The region is then
//! merged from its start, with the events the document holds gone through first and only
//! for what they mean to the others.

use crate::oplog::{ByteRange, OpIndex, OpKind, OpLog, OpSlice};
use crate::shift::Shifter;
use crate::tracker::{ItemId, Targets, Tracker};
use std::ops::Range;
use strider_graph::{BlockId, EventId, Move, Segment, Walker};
use strider_text::PieceTable;

/// Where merging put each op of the regions. The rest of each edit is the op itself, in the
/// log.
#[derive(Default)]
pub(crate) struct Placed {
    /// The stretches of blocks, in the order they were merged. The ops of a stretch are taken
    /// together, so one that runs on from a block into the next is a single op.
    order: Vec<Range<BlockId>>,
    /// For each op of those stretches in turn. An insert: the index it went in at. A delete:
    /// the index and length of each run of characters it removed, then how many runs.
    at: Vec<u32>,
    /// How many runs the delete being recorded has removed so far.
    runs: u32,
    /// Length of the document so far.
    len: u32,
    /// The first block with events that the document does not hold yet. Where the ops of
    /// earlier blocks go is not recorded.
    new: BlockId,
    /// Whether the stretch being merged is of those earlier blocks.
    held: bool,
}

impl Placed {
    /// Starts on the ops of a stretch of blocks, all of which the document holds or none.
    pub(crate) fn begin(&mut self, stretch: Range<BlockId>) {
        self.held = stretch.end <= self.new;
        debug_assert!(self.held || stretch.start >= self.new, "a stretch with events on both sides of the base");
        if !self.held {
            self.order.push(stretch);
        }
    }

    /// Records that an insert of `len` characters went in at `pos`.
    pub(crate) fn insert(&mut self, pos: u32, len: u32) {
        if !self.held {
            self.len += len;
            self.at.push(pos);
        }
    }

    /// Records that the delete being merged removed the `len` characters at `pos`.
    pub(crate) fn remove(&mut self, pos: u32, len: u32) {
        if !self.held {
            self.len -= len;
            self.at.extend([pos, len]);
            self.runs += 1;
        }
    }

    /// Ends a delete, once everything it removed is recorded.
    pub(crate) fn end_delete(&mut self) {
        if !self.held {
            self.at.push(std::mem::take(&mut self.runs));
        }
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
    resolved: Vec<ItemId>,
}

const DESCENDING: u32 = 1 << 31;

/// The document with the first `events` events merged, which a merge of the rest starts from.
#[derive(Default)]
pub(crate) struct Base {
    pub events: EventId,
    /// The length of `text` in characters.
    pub chars: u32,
    pub text: String,
}

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
    starts: Vec<EventId>,
    segments: Vec<Segment>,
    /// For each block, the index of the op containing its first event; then the number of ops.
    first_op: Vec<OpIndex>,
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
    fn take_kept(&mut self, start: EventId) -> Kept {
        match self.starts.binary_search(&start) {
            Ok(at) => std::mem::take(&mut self.kept[at]),
            Err(_) => Kept::default(),
        }
    }

    /// Iterates over the ops of a stretch of blocks. An op that runs past either end is cut
    /// there.
    #[inline]
    fn ops<'a>(&self, log: &'a OpLog, stretch: &Range<BlockId>) -> impl DoubleEndedIterator<Item = OpSlice> + 'a {
        self.ops_from(log, stretch, log.graph.blocks().start(stretch.start))
    }

    /// Iterates over the ops of a stretch of blocks from event `from` on, which is in it.
    #[inline]
    fn ops_from<'a>(&self, log: &'a OpLog, stretch: &Range<BlockId>, from: EventId) -> impl DoubleEndedIterator<Item = OpSlice> + 'a {
        log.ops(self.op_at(log, stretch, from)..self.first_op[stretch.end as usize], from..log.graph.blocks().start(stretch.end))
    }

    /// Finds the op containing event `from` of a stretch of blocks.
    fn op_at(&self, log: &OpLog, stretch: &Range<BlockId>, from: EventId) -> OpIndex {
        if from == log.graph.blocks().start(stretch.start) {
            self.first_op[stretch.start as usize]
        } else {
            log.op_containing(from)
        }
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

    /// Notes that the log's blocks are no longer those the last checkout saw, though its
    /// events are.
    pub(crate) fn outdate(&mut self) {
        if let Some(plan) = &mut self.plan {
            plan.events = usize::MAX;
        }
    }
}

/// Merges the log's events after `base` into the document that `base` holds, reusing and
/// updating `memo`. Returns the text and its length in characters. Where `base` ends inside
/// a region, a block and an op must start there.
pub(crate) fn checkout(log: &OpLog, memo: &mut Memo, base: &Base) -> (String, u32) {
    // The log only ever grows, so the same length means the same events.
    if memo.plan.as_ref().is_none_or(|plan| plan.events != log.len()) {
        memo.plan = Some(Plan::new(log, memo.plan.take()));
    }
    let plan = memo.plan.as_mut().expect("a plan was just made");
    let mut kept = std::mem::take(&mut plan.kept);

    // Forwards: merge the regions. The events of a linear stretch are edits already, so all
    // that is needed of them for now is what they do to the document's length.
    let blocks = log.graph.blocks();
    let events = |stretch: &Range<BlockId>| blocks.start(stretch.start)..blocks.start(stretch.end);
    debug_assert!((log.graph.floor()..log.graph.len()).contains(&base.events), "a base the log does not reach back to");
    let new = blocks.containing(base.events);
    let mut document = Placed { order: Vec::with_capacity(memo.room.0), at: Vec::with_capacity(memo.room.1), runs: 0, len: base.chars, new, held: false };
    let mut workspace = Workspace::default();
    // The segments that have events after the base, and for each the first of those.
    let after = |segment: &Segment| Some(events(segment.blocks())).filter(|events| events.end > base.events).map(|events| events.start.max(base.events));
    for (segment, kept) in plan.segments.iter().zip(&mut kept) {
        let Some(from) = after(segment) else { continue };
        match segment {
            Segment::Linear(linear) => {
                // A history with no concurrency is one stretch, and the log keeps its count.
                let whole = plan.segments.len() == 1 && from == blocks.start(0);
                let (inserted, removed) = if whole { log.total_lengths() } else { log.lengths(plan.op_at(log, linear, from), from..blocks.start(linear.end)) };
                document.len = document.len + inserted - removed;
            }
            Segment::Region(region) => {
                debug_assert_eq!(from, blocks.start(new.max(region.start)), "a base partway through a block of a region");
                merge_region(log, plan, region, kept, &mut workspace, &mut document)
            }
        }
    }
    // The text is built without the room that merging took.
    drop(workspace);
    plan.kept = kept;
    memo.room = (document.order.len(), document.at.len());

    // Backwards: take every edit back, starting from the final document.
    let mut text = PieceTable::new(document.len, log.content());
    let bytes = |(start, end): ByteRange| start as usize..end as usize;
    let mut order = document.order.iter().rev().peekable();
    let mut places = document.at.iter().rev().copied();
    let mut place = || places.next().expect("a place for every op of a region");
    let mut trims: Vec<OpSlice> = Vec::new();
    for segment in plan.segments.iter().rev() {
        let Some(from) = after(segment) else { break };
        match segment {
            Segment::Linear(linear) => {
                for op in plan.ops_from(log, linear, from).rev() {
                    // A delete that takes back the end of the insert before it is met first.
                    // The two together are a shorter insert, so it only has to be counted.
                    if op.trims {
                        trims.push(op);
                        continue;
                    }
                    let cut = trims.iter().map(|trim| trim.len).sum::<u32>();
                    let folds =
                        cut > 0 && op.kind == OpKind::Ins && cut <= op.len && trims.last().is_some_and(|first| first.first_event == op.first_event + op.len);
                    if cut > 0 && !folds {
                        // Not enough of the insert is here: this stretch begins partway
                        // through it.
                        trims.drain(..).for_each(|trim| text.undo_remove(trim.pos, trim.len));
                    }
                    match op.kind {
                        OpKind::Ins if folds => {
                            text.undo_insert(op.pos, bytes(log.trimmed(op.text, op.len, cut)), op.len - cut);
                            trims.clear();
                        }
                        OpKind::Ins => text.undo_insert(op.pos, bytes(op.text), op.len),
                        OpKind::Del => text.undo_remove(op.pos, op.len),
                    }
                }
                trims.drain(..).for_each(|trim| text.undo_remove(trim.pos, trim.len));
            }
            Segment::Region(region) => {
                while let Some(stretch) = order.next_if(|stretch| stretch.start >= region.start) {
                    for op in plan.ops(log, stretch).rev() {
                        match op.kind {
                            OpKind::Ins => text.undo_insert(place(), bytes(op.text), op.len),
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
    // What is left is the document at the base, whose text is already known.
    text.undo_rest(&base.text);
    (text.into_string(), document.len)
}

/// Merges one region, recording in `document` where its ops go.
fn merge_region(log: &OpLog, plan: &Plan, region: &Range<BlockId>, kept: &mut Kept, workspace: &mut Workspace, document: &mut Placed) {
    let Workspace { tracker, walker, shifter } = workspace;
    // Nothing kept: the region has not needed the tracker so far. See whether it does now.
    if kept.events == 0 && shifter.merge(log, log.graph.blocks(), &plan.first_op, region, document) {
        return;
    }
    let blocks = log.graph.blocks();
    let (start, end) = (blocks.start(region.start), blocks.start(region.end));
    let goes_before = |a: EventId, b: EventId| log.inserts_before(start + a, start + b);
    // The region's blocks that the document already holds, which are its first. The document
    // was no longer when the region began than it is now plus what those events removed.
    let held = document.new.clamp(region.start, region.end);
    tracker.begin_region((end - start) as usize, document.len + (blocks.start(held) - start));

    // Blocks resolved in full by earlier replays are placed by id. Their events stay in the
    // tracker's prepare version, which is where the walk of the rest begins.
    let known_blocks = (region.start..region.end).take_while(|&block| blocks.start(block + 1) - start <= kept.events).count();
    let known = region.start..region.start + known_blocks as BlockId;
    let mut resolved = kept.resolved.iter().copied();
    let mut next = || resolved.next().expect("something kept for every resolved event");
    let parted = held.min(known.end);
    for stretch in [known.start..parted, parted..known.end] {
        if stretch.is_empty() {
            continue;
        }
        document.begin(stretch.clone());
        for op in plan.ops(log, &stretch) {
            let event = op.first_event - start;
            match op.kind {
                OpKind::Ins => {
                    let (left, right) = (next(), next());
                    let pos = tracker.insert_by_id(event, op.len, left, right, goes_before);
                    document.insert(pos, op.len);
                }
                OpKind::Del => {
                    let mut done = 0;
                    while done < op.len {
                        let (first, stretch) = (next(), next());
                        let (n, descending) = ((stretch & !DESCENDING).min(op.len - done), stretch & DESCENDING != 0);
                        let item = if descending { first + 1 - n } else { first };
                        tracker.delete_by_id(Targets { first_event: event + done, len: n, item, descending }, |pos, len| document.remove(pos, len));
                        done += n;
                    }
                    document.end_delete();
                }
            }
        }
    }
    if known_blocks == region.len() {
        return;
    }

    // What the document holds is gone through before anything else, so that where the rest
    // goes is a place in that document.
    walker.walk(blocks, region, known_blocks, (held - region.start) as usize, |step| match step {
        Move::Retreat(stretch) => {
            for op in plan.ops(log, &stretch) {
                tracker.retreat(op.kind, op.first_event - start..op.first_event - start + op.len);
            }
        }
        Move::Advance(stretch) => {
            for op in plan.ops(log, &stretch) {
                tracker.advance(op.kind, op.first_event - start..op.first_event - start + op.len);
            }
        }
        Move::Apply(block) => {
            document.begin(block..block + 1);
            for op in plan.ops(log, &(block..block + 1)) {
                walk_op(start, op, tracker, &goes_before, document);
            }
        }
    });

    // The tracker now knows what every event of the region resolved to. Keep that.
    kept.events = end - start;
    kept.resolved.clear();
    for op in plan.ops(log, region) {
        let event = op.first_event - start;
        match op.kind {
            OpKind::Ins => kept.resolved.extend(tracker.origins(event)),
            OpKind::Del => {
                let mut done = 0;
                while done < op.len {
                    let targets = tracker.targets(event + done, event + op.len);
                    kept.resolved.extend([targets.first(), targets.len | if targets.descending { DESCENDING } else { 0 }]);
                    done += targets.len;
                }
            }
        }
    }
    kept.resolved.shrink_to_fit();
}

/// Merges one op at the tracker's prepare version.
fn walk_op(start: EventId, op: OpSlice, tracker: &mut Tracker, goes_before: &impl Fn(EventId, EventId) -> bool, document: &mut Placed) {
    let event = op.first_event - start;
    match op.kind {
        OpKind::Ins => {
            let pos = tracker.insert(event, op.len, op.pos, goes_before);
            document.insert(pos, op.len);
        }
        OpKind::Del => {
            tracker.delete(event, op.pos, op.len, op.forward, |pos, len| document.remove(pos, len));
            document.end_delete();
        }
    }
}
