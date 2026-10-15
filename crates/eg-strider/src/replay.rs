use crate::crdt_records::{CrdtRecords, DeleteTargets, RecordId};
use crate::oplog::{ByteRange, OpIndex, OpKind, OpLog, OpSlice};
use crate::shift::Shifter;
use std::ops::Range;
use strider_graph::{walk, EventIndex, EventRunIndex, HistoryRange, WalkScratch, WalkStep};
use strider_text::PieceTable;

/// Where merging put each op of the regions. The rest of each edit is the op itself.
#[derive(Default)]
pub(crate) struct Placed {
    /// Stretches of blocks, in merge order. An op that runs from one block into the next is one
    /// op.
    order: Vec<Range<EventRunIndex>>,
    /// Per op of those stretches. An insert: the index it went in at. A delete: the index and
    /// length of each run it removed, then the number of runs.
    at: Vec<u32>,
    /// Runs the current delete has removed so far.
    runs: u32,
    /// Length of the document so far.
    len: u32,
    /// The first block whose events the document does not hold. Ops of earlier blocks are not
    /// recorded.
    new: EventRunIndex,
    /// Whether the current stretch is of those earlier blocks.
    held: bool,
}

impl Placed {
    /// Starts on a stretch of blocks, which the document holds all of or none of.
    pub(crate) fn begin(&mut self, stretch: Range<EventRunIndex>) {
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

    /// Records that the current delete removed the `len` characters at `pos`.
    pub(crate) fn remove(&mut self, pos: u32, len: u32) {
        if !self.held {
            self.len -= len;
            self.at.extend([pos, len]);
            self.runs += 1;
        }
    }

    /// Ends a delete once all it removed is recorded.
    pub(crate) fn end_delete(&mut self) {
        if !self.held {
            self.at.push(std::mem::take(&mut self.runs));
        }
    }

    /// Marks where things stand, for [`undo_to`](Self::undo_to).
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

/// What a region's critical version keeps once the region has been through the CRDT records.
#[derive(Default)]
struct Kept {
    /// How many of the region's events, from its start, `resolved` covers.
    events: u32,
    /// Records, op by op in log order. An insert: its first character's left origin, then
    /// the right origin all its characters share. A delete: per stretch of consecutive targets,
    /// the first and the count, with [`DESCENDING`] if they count down.
    resolved: Vec<RecordId>,
}

const DESCENDING: u32 = 1 << 31;

/// The document with the first `events` events merged, which a merge of the rest starts from.
#[derive(Default)]
pub(crate) struct Base {
    pub events: EventIndex,
    /// The length of `text` in characters.
    pub chars: u32,
    pub text: String,
}

/// What earlier replays of a log worked out.
#[derive(Default)]
pub(crate) struct Memo {
    plan: Option<Plan>,
    /// How many stretches and places the regions took last time, to reserve next time.
    room: (usize, usize),
}

/// Scratch space for merging regions, reused across regions.
#[derive(Default)]
struct Workspace {
    records: CrdtRecords,
    walk: WalkScratch,
    shifter: Shifter,
}

/// The history's shape, valid while the log has the same number of events.
struct Plan {
    events: usize,
    /// The first event of each segment.
    starts: Vec<EventIndex>,
    segments: Vec<HistoryRange>,
    /// Per block, the op holding its first event; then the number of ops.
    first_op: Vec<OpIndex>,
    /// Per segment, what its starting critical version keeps: nothing unless it is a region
    /// that went through the CRDT records.
    kept: Vec<Kept>,
}

impl Plan {
    /// Analyses the log, taking over what the `previous` plan kept for regions that start at
    /// the same event.
    fn new(log: &OpLog, mut previous: Option<Plan>) -> Self {
        let blocks = log.graph.runs();
        let segments = log.graph.split_at_critical_versions();
        let starts = segments.iter().map(|segment| blocks.start(segment.runs().start)).collect();
        let first_op = log.ops_containing((0..=blocks.len()).map(|block| blocks.start(block)));
        let kept = segments
            .iter()
            .map(|segment| match (segment, &mut previous) {
                (HistoryRange::Concurrent(region), Some(previous)) => previous.take_kept(blocks.start(region.start)),
                _ => Kept::default(),
            })
            .collect();
        Plan { events: log.len(), starts, segments, first_op, kept }
    }

    /// Takes what is kept for the region starting at `start`, if anything.
    fn take_kept(&mut self, start: EventIndex) -> Kept {
        match self.starts.binary_search(&start) {
            Ok(at) => std::mem::take(&mut self.kept[at]),
            Err(_) => Kept::default(),
        }
    }

    /// Iterates over a stretch's ops, cutting any that run past either end.
    #[inline]
    fn ops<'a>(&self, log: &'a OpLog, stretch: &Range<EventRunIndex>) -> impl DoubleEndedIterator<Item = OpSlice> + 'a {
        self.ops_from(log, stretch, log.graph.runs().start(stretch.start))
    }

    /// Iterates over a stretch's ops from event `from`, which is in it.
    #[inline]
    fn ops_from<'a>(&self, log: &'a OpLog, stretch: &Range<EventRunIndex>, from: EventIndex) -> impl DoubleEndedIterator<Item = OpSlice> + 'a {
        log.ops(self.op_at(log, stretch, from)..self.first_op[stretch.end as usize], from..log.graph.runs().start(stretch.end))
    }

    /// Returns the op holding event `from` of a stretch.
    fn op_at(&self, log: &OpLog, stretch: &Range<EventRunIndex>, from: EventIndex) -> OpIndex {
        if from == log.graph.runs().start(stretch.start) {
            self.first_op[stretch.start as usize]
        } else {
            log.op_containing(from)
        }
    }

    /// Counts the regions that have been through the CRDT records.
    fn tracked_regions(&self) -> usize {
        self.kept.iter().filter(|kept| kept.events > 0).count()
    }
}

impl Memo {
    /// Returns how many regions the last checkout could not merge by shifting alone.
    pub(crate) fn tracked_regions(&self) -> usize {
        self.plan.as_ref().map_or(0, Plan::tracked_regions)
    }

    /// Notes that the log's blocks changed though its events did not.
    pub(crate) fn outdate(&mut self) {
        if let Some(plan) = &mut self.plan {
            plan.events = usize::MAX;
        }
    }
}

/// Merges the log's events after `base` into the document `base` holds, reusing and updating
/// `memo`. Returns the text and its length in characters.
///
/// Linear stretches apply as recorded. A region is merged by shifting if it can be, else with
/// the CRDT records, whose results are memoised at the region's critical version. Merging only works
/// out where each op goes; the text is then built by taking the edits back, last to first. If
/// `base` ends inside a region, a block and an op must start there: the region's events that
/// `base` holds are walked first and not recorded.
pub(crate) fn checkout(log: &OpLog, memo: &mut Memo, base: &Base) -> (String, u32) {
    // The log only ever grows, so the same length means the same events.
    if memo.plan.as_ref().is_none_or(|plan| plan.events != log.len()) {
        memo.plan = Some(Plan::new(log, memo.plan.take()));
    }
    let plan = memo.plan.as_mut().expect("a plan was just made");
    let mut kept = std::mem::take(&mut plan.kept);

    // Forwards: merge the regions. The events of a linear stretch are edits already, so all
    // that is needed of them for now is what they do to the document's length.
    let blocks = log.graph.runs();
    let events = |stretch: &Range<EventRunIndex>| blocks.start(stretch.start)..blocks.start(stretch.end);
    debug_assert!((log.graph.first_held_event()..log.graph.len()).contains(&base.events), "a base the log does not reach back to");
    let new = blocks.containing(base.events);
    let mut document = Placed { order: Vec::with_capacity(memo.room.0), at: Vec::with_capacity(memo.room.1), runs: 0, len: base.chars, new, held: false };
    let mut workspace = Workspace::default();
    // The segments that have events after the base, and for each the first of those.
    let after = |segment: &HistoryRange| Some(events(segment.runs())).filter(|events| events.end > base.events).map(|events| events.start.max(base.events));
    for (segment, kept) in plan.segments.iter().zip(&mut kept) {
        let Some(from) = after(segment) else { continue };
        match segment {
            HistoryRange::Linear(linear) => {
                // A history with no concurrency is one stretch, and the log keeps its count.
                let whole = plan.segments.len() == 1 && from == blocks.start(0);
                let (inserted, removed) = if whole { log.total_lengths() } else { log.lengths(plan.op_at(log, linear, from), from..blocks.start(linear.end)) };
                document.len = document.len + inserted - removed;
            }
            HistoryRange::Concurrent(region) => {
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
            HistoryRange::Linear(linear) => {
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
            HistoryRange::Concurrent(region) => {
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
fn merge_region(log: &OpLog, plan: &Plan, region: &Range<EventRunIndex>, kept: &mut Kept, workspace: &mut Workspace, document: &mut Placed) {
    let Workspace { records, walk: walk_scratch, shifter } = workspace;
    // Nothing kept: the region has not needed the CRDT records so far. See whether it does now.
    if kept.events == 0 && shifter.merge(log, log.graph.runs(), &plan.first_op, region, document) {
        return;
    }
    let blocks = log.graph.runs();
    let (start, end) = (blocks.start(region.start), blocks.start(region.end));
    let goes_before = |a: EventIndex, b: EventIndex| log.inserts_before(start + a, start + b);
    // The region's blocks that the document already holds, which are its first. The document
    // was no longer when the region began than it is now plus what those events removed.
    let held = document.new.clamp(region.start, region.end);
    records.clear_to_placeholder((end - start) as usize, document.len + (blocks.start(held) - start));

    // EventRuns resolved in full by earlier replays are placed by id. Their events stay in the
    // records's prepare version, which is where the walk of the rest begins.
    let known_blocks = (region.start..region.end).take_while(|&block| blocks.start(block + 1) - start <= kept.events).count();
    let known = region.start..region.start + known_blocks as EventRunIndex;
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
                    let pos = records.insert_with_known_origins(event, op.len, left, right, goes_before);
                    document.insert(pos, op.len);
                }
                OpKind::Del => {
                    let mut done = 0;
                    while done < op.len {
                        let (first, stretch) = (next(), next());
                        let (n, descending) = ((stretch & !DESCENDING).min(op.len - done), stretch & DESCENDING != 0);
                        let record = if descending { first + 1 - n } else { first };
                        records.delete_with_known_targets(DeleteTargets { first_event: event + done, len: n, record, descending }, |pos, len| {
                            document.remove(pos, len)
                        });
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
    walk(walk_scratch, blocks, region, known_blocks, (held - region.start) as usize, |step| match step {
        WalkStep::Retreat(stretch) => {
            for op in plan.ops(log, &stretch) {
                records.retreat(op.kind, op.first_event - start..op.first_event - start + op.len);
            }
        }
        WalkStep::Advance(stretch) => {
            for op in plan.ops(log, &stretch) {
                records.advance(op.kind, op.first_event - start..op.first_event - start + op.len);
            }
        }
        WalkStep::Apply(block) => {
            document.begin(block..block + 1);
            for op in plan.ops(log, &(block..block + 1)) {
                walk_op(start, op, records, &goes_before, document);
            }
        }
    });

    // The records now knows what every event of the region resolved to. Keep that.
    kept.events = end - start;
    kept.resolved.clear();
    for op in plan.ops(log, region) {
        let event = op.first_event - start;
        match op.kind {
            OpKind::Ins => kept.resolved.extend(records.origins_of(event)),
            OpKind::Del => {
                let mut done = 0;
                while done < op.len {
                    let targets = records.targets_of(event + done, event + op.len);
                    kept.resolved.extend([targets.first(), targets.len | if targets.descending { DESCENDING } else { 0 }]);
                    done += targets.len;
                }
            }
        }
    }
    kept.resolved.shrink_to_fit();
}

/// Merges one op at the CRDT records' prepare version.
fn walk_op(start: EventIndex, op: OpSlice, records: &mut CrdtRecords, goes_before: &impl Fn(EventIndex, EventIndex) -> bool, document: &mut Placed) {
    let event = op.first_event - start;
    match op.kind {
        OpKind::Ins => {
            let pos = records.insert(event, op.len, op.pos, goes_before);
            document.insert(pos, op.len);
        }
        OpKind::Del => {
            records.delete(event, op.pos, op.len, op.forward, |pos, len| document.remove(pos, len));
            document.end_delete();
        }
    }
}
