mod walk;

pub use walk::{Move, Walker};

use std::ops::Range;

/// Replica local version number/index for an event.
pub type EventIndex = u32;

/// Index of a block among those the graph holds, in order of their first events. Shifts by one
/// when an earlier block splits.
pub type BlockIndex = u32;

/// The event graph of a document's history: which events each event's author had seen. Merging
/// is only needed where history is concurrent, so the graph is stored run-length encoded, cut
/// into blocks, with the critical versions that every path passes through kept up to date.
#[derive(Default)]
pub struct Graph {
    blocks: Blocks,
    /// Events nothing descends from yet, ascending.
    frontier: Vec<EventIndex>,
    /// Per block, how many events from the start of history it has seen: exact if it has seen
    /// all before it, else a lower bound.
    seen: Vec<EventIndex>,
    /// Blocks that start at a critical version, ascending.
    critical: Vec<BlockIndex>,
    /// Scratch: the blocks a new event's parents end.
    ends: Vec<BlockIndex>,
}

impl Graph {
    /// Creates an empty graph.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a graph of only the events after the first `events`. Every later event must have
    /// seen `frontier`, the frontier of those first events.
    pub fn after(events: EventIndex, frontier: &[EventIndex]) -> Self {
        Graph { blocks: Blocks { start: vec![events], ..Blocks::default() }, frontier: frontier.to_vec(), ..Self::default() }
    }

    /// Returns the first event the graph holds.
    pub fn floor(&self) -> EventIndex {
        self.blocks.start[0]
    }

    /// Returns the number of events.
    pub fn len(&self) -> EventIndex {
        self.blocks.start(self.blocks.len())
    }

    /// Tells whether the graph has no events.
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Returns the events nothing descends from yet, ascending.
    pub fn frontier(&self) -> &[EventIndex] {
        &self.frontier
    }

    /// Tells whether an event with these parents just extends the last event.
    pub fn continues(&self, parents: &[EventIndex]) -> bool {
        !self.is_empty() && parents == [self.len() - 1]
    }

    /// If some later event has not seen every event before `at`, starts a block at `at` and
    /// returns true.
    pub fn cut(&mut self, at: EventIndex) -> bool {
        if at <= self.floor() || at >= self.len() {
            return false;
        }

        let block = self.blocks.containing(at);
        let critical = |block: BlockIndex| self.critical.binary_search(&block).is_ok();
        if self.blocks.start(block) == at {
            return !critical(block);
        }

        // Partway along a block, every later event has what comes before unless something is
        // concurrent with the block.
        if critical(block) && (block + 1 == self.blocks.len() || critical(block + 1)) {
            return false;
        }

        self.split(block, at);
        true
    }

    /// Returns the graph's blocks.
    pub fn blocks(&self) -> &Blocks {
        &self.blocks
    }

    /// Appends a chain of `len` events, the first with `parents`. Returns their ids. `len` must
    /// not be 0, so that there are never more blocks than events, and `parents` must be known
    /// events in ascending order.
    pub fn push(&mut self, parents: &[EventIndex], len: u32) -> Range<EventIndex> {
        let start = self.len();
        debug_assert!(len > 0, "a run of no events");
        debug_assert!(parents.is_sorted_by(|a, b| a < b), "parents not in ascending order");
        debug_assert!(parents.iter().all(|&parent| parent < start), "a parent is not a known event");
        let end = start.checked_add(len).expect("too many events");
        if self.continues(parents) {
            // Nothing else can have the last event as a parent yet, so its block just grows,
            // and the block's new end replaces the last event of the frontier.
            *self.blocks.start.last_mut().expect("a block to continue") = end;
            *self.frontier.last_mut().expect("the last event is in the frontier") = end - 1;
            return start..end;
        }

        // A parent part-way along a block splits it there. Taking them in ascending order keeps
        // the indexes already in `ends` valid, since a split only moves the blocks after it.
        let mut ends = std::mem::take(&mut self.ends);
        ends.clear();
        let floor = self.floor();
        // Parents from before the graph begins are in every event's history, and name no block.
        for &parent in parents.iter().filter(|&&parent| parent >= floor) {
            let block = self.blocks.containing(parent);
            if parent + 1 < self.blocks.start(block + 1) {
                self.split(block, parent + 1);
            }
            ends.push(block);
        }
        // An event whose parents are the whole frontier has seen everything, and becomes the
        // whole frontier.
        let merges_all = parents == self.frontier;
        let seen = if merges_all { start } else { ends.iter().map(|&block| self.seen_after(block)).max().unwrap_or(floor) };
        let new = self.blocks.len();
        self.blocks.parents.extend_from_slice(&ends);
        self.blocks.parent_start.push(self.blocks.parents.len() as u32);
        self.blocks.start.push(end);
        self.seen.push(seen);
        self.ends = ends;

        // A version is critical only while every later block has seen all that came before
        // it.
        while self.critical.last().is_some_and(|&block| self.blocks.start(block) > seen) {
            self.critical.pop();
        }
        if seen == start {
            self.critical.push(new);
        }
        if merges_all {
            self.frontier.clear();
        } else {
            self.frontier.retain(|event| !parents.contains(event));
        }
        self.frontier.push(end - 1);
        start..end
    }

    /// Returns how many events from the start of history a block's last event has seen.
    fn seen_after(&self, block: BlockIndex) -> EventIndex {
        let i = block as usize;
        if self.seen[i] == self.blocks.start[i] {
            self.blocks.start[i + 1]
        } else {
            self.seen[i]
        }
    }

    /// Splits a block so that a new one starts at `at`.
    fn split(&mut self, block: BlockIndex, at: EventIndex) {
        let blocks = &mut self.blocks;
        let (first, second, i) = (block, block + 1, block as usize);
        // Later blocks named this one for its last event, which the second part now has;
        // and every block after it moves up one.
        let later = blocks.parent_start[i + 1] as usize;
        for parent in &mut blocks.parents[later..] {
            *parent += BlockIndex::from(*parent >= first);
        }
        blocks.parents.insert(later, first);
        blocks.start.insert(i + 1, at);
        blocks.parent_start.insert(i + 1, later as u32);
        for start in &mut blocks.parent_start[i + 2..] {
            *start += 1;
        }
        self.seen.insert(i + 1, if self.seen[i] == blocks.start[i] { at } else { self.seen[i] });

        let moved = self.critical.partition_point(|&critical| critical <= first);
        for critical in &mut self.critical[moved..] {
            *critical += 1;
        }
        if self.seen[i + 1..].iter().all(|&seen| seen >= at) {
            self.critical.insert(moved, second);
        }
    }

    /// Splits the history at its critical versions.
    pub fn segments(&self) -> Vec<Segment> {
        let blocks = self.blocks.len();
        let next = |i: usize| self.critical.get(i + 1).copied().unwrap_or(blocks);
        let mut segments = Vec::new();
        let mut i = 0;
        while i < self.critical.len() {
            let from = self.critical[i];
            if next(i) == from + 1 {
                // Critical on both sides: nothing is concurrent with this block. Take
                // every block that follows in the same way.
                while i + 1 < self.critical.len() && next(i + 1) == self.critical[i + 1] + 1 {
                    i += 1;
                }
                segments.push(Segment::Linear(from..next(i)));
            } else {
                segments.push(Segment::Region(from..next(i)));
            }
            i += 1;
        }
        segments
    }
}

/// The graph cut into blocks: maximal chains of events that nothing branches off partway, so a
/// version holds a block whole or not at all.
pub struct Blocks {
    /// Block `i` covers events `start[i]..start[i + 1]`.
    start: Vec<EventIndex>,
    parent_start: Vec<u32>,
    /// Each block's parent blocks: those its first event's parents end.
    parents: Vec<BlockIndex>,
}

impl Default for Blocks {
    fn default() -> Self {
        Blocks { start: vec![0], parent_start: vec![0], parents: Vec::new() }
    }
}

/// A stretch of history between two critical versions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Segment {
    /// Blocks nothing is concurrent with: their events apply as recorded.
    Linear(Range<BlockIndex>),
    /// Blocks with concurrency among them, which have to be merged.
    Region(Range<BlockIndex>),
}

impl Segment {
    /// Returns the blocks in the segment.
    pub fn blocks(&self) -> &Range<BlockIndex> {
        match self {
            Segment::Linear(blocks) | Segment::Region(blocks) => blocks,
        }
    }
}

impl Blocks {
    /// Returns the number of blocks.
    pub fn len(&self) -> BlockIndex {
        (self.parent_start.len() - 1) as BlockIndex
    }

    /// Tells whether there are no blocks.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns a block's first event. One past the last block gives the number of events.
    pub fn start(&self, block: BlockIndex) -> EventIndex {
        self.start[block as usize]
    }

    /// Returns the events in a block.
    pub fn events(&self, block: BlockIndex) -> Range<EventIndex> {
        self.start(block)..self.start(block + 1)
    }

    /// Returns the blocks that a block's first event has as parents.
    pub fn parents(&self, block: BlockIndex) -> &[BlockIndex] {
        let i = block as usize;
        &self.parents[self.parent_start[i] as usize..self.parent_start[i + 1] as usize]
    }

    /// Finds the block holding an event.
    pub fn containing(&self, event: EventIndex) -> BlockIndex {
        // Parents are mostly recent events, so the search widens backwards from the last block.
        let (mut end, mut step) = (self.len() as usize, 1);
        while step < end && self.start[end - step] > event {
            end -= step;
            step *= 2;
        }
        let from = end.saturating_sub(step);
        (from + self.start[from..end].partition_point(|&start| start <= event) - 1) as BlockIndex
    }
}
