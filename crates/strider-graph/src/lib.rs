mod walk;

pub use walk::{Move, Walker};

use std::ops::Range;

/// Replica local version number/index for an event.
pub type EventId = u32;

/// Index of a block, in order of the blocks' first events.
pub type BlockId = u32;

/// The event graph of a document's history: which events each event's author had seen. Merging
/// is only needed where history is concurrent, so the graph is stored run-length encoded, cut
/// into blocks, with the critical versions that every path passes through kept up to date.
#[derive(Default)]
pub struct Graph {
    blocks: Blocks,
    /// Events nothing descends from yet, ascending.
    heads: Vec<EventId>,
    /// Per block, how many events from the start of history it has seen: exact if it has seen
    /// all before it, else a lower bound.
    seen: Vec<EventId>,
    /// Blocks that start at a critical version, ascending.
    critical: Vec<BlockId>,
    /// Scratch: the blocks a new event's parents end.
    ends: Vec<BlockId>,
}

impl Graph {
    /// Creates an empty graph.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a graph of only the events after the first `events`. Every later event must have
    /// seen `heads`, the heads of those first events.
    pub fn after(events: EventId, heads: &[EventId]) -> Self {
        Graph { blocks: Blocks { start: vec![events], ..Blocks::default() }, heads: heads.to_vec(), ..Self::default() }
    }

    /// Returns the first event the graph holds.
    pub fn floor(&self) -> EventId {
        self.blocks.start[0]
    }

    /// Returns the number of events.
    pub fn len(&self) -> EventId {
        self.blocks.start(self.blocks.len())
    }

    /// Tells whether the graph has no events.
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Returns the events nothing descends from yet, ascending.
    pub fn frontier(&self) -> &[EventId] {
        // Kept in order: each new event is the highest so far, and goes on the end.
        &self.heads
    }

    /// Tells whether an event with these parents just extends the last event.
    pub fn continues(&self, parents: &[EventId]) -> bool {
        !self.is_empty() && parents == [self.len() - 1]
    }

    /// If some later event has not seen every event before `at`, starts a block at `at` and
    /// returns true.
    pub fn cut(&mut self, at: EventId) -> bool {
        if at <= self.floor() || at >= self.len() {
            return false;
        }
        let block = self.blocks.containing(at);
        let critical = |block: BlockId| self.critical.binary_search(&block).is_ok();
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

    /// Appends a chain of `len` events, the first with `parents`. Returns their ids.
    pub fn push(&mut self, parents: &[EventId], len: u32) -> Range<EventId> {
        // A block with no events would let there be more blocks than events.
        assert!(len > 0, "a run of no events");
        let start = self.len();
        let end = start.checked_add(len).expect("too many events");
        if self.continues(parents) {
            // Nothing else can have the last event as a parent yet, so its block just grows,
            // and the block's new end is the last head in its place.
            *self.blocks.start.last_mut().expect("a block to continue") = end;
            *self.heads.last_mut().expect("the last event is a head") = end - 1;
            return start..end;
        }
        assert!(parents.iter().all(|&p| p < start), "parent is not a known event");
        // Parents usually arrive in order; only otherwise are they copied to be sorted.
        let sorted: Vec<EventId>;
        let parents = if parents.windows(2).all(|pair| pair[0] < pair[1]) {
            parents
        } else {
            let mut copy = parents.to_vec();
            copy.sort_unstable();
            copy.dedup();
            sorted = copy;
            &sorted
        };

        // A parent part-way along a block splits it there. Taking them in ascending order
        // keeps the blocks already found where they were.
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
        // An event that merges every head has seen all there is, and leaves no other head.
        let merges_all = parents == self.heads;
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
            self.heads.clear();
        } else {
            self.heads.retain(|h| !parents.contains(h));
        }
        self.heads.push(end - 1);
        start..end
    }

    /// Returns how many events from the start of history a block's last event has seen.
    fn seen_after(&self, block: BlockId) -> EventId {
        let i = block as usize;
        if self.seen[i] == self.blocks.start[i] {
            self.blocks.start[i + 1]
        } else {
            self.seen[i]
        }
    }

    /// Splits a block so that a new one starts at `at`.
    fn split(&mut self, block: BlockId, at: EventId) {
        let blocks = &mut self.blocks;
        let (first, second, i) = (block, block + 1, block as usize);
        // Later blocks named this one for its last event, which the second part now has;
        // and every block after it moves up one.
        let later = blocks.parent_start[i + 1] as usize;
        for parent in &mut blocks.parents[later..] {
            *parent += BlockId::from(*parent >= first);
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
    start: Vec<EventId>,
    parent_start: Vec<u32>,
    /// Each block's parent blocks: those its first event's parents end.
    parents: Vec<BlockId>,
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
    Linear(Range<BlockId>),
    /// Blocks with concurrency among them, which have to be merged.
    Region(Range<BlockId>),
}

impl Segment {
    /// Returns the blocks in the segment.
    pub fn blocks(&self) -> &Range<BlockId> {
        match self {
            Segment::Linear(blocks) | Segment::Region(blocks) => blocks,
        }
    }
}

impl Blocks {
    /// Returns the number of blocks.
    pub fn len(&self) -> BlockId {
        (self.parent_start.len() - 1) as BlockId
    }

    /// Tells whether there are no blocks.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns a block's first event. One past the last block gives the number of events.
    pub fn start(&self, block: BlockId) -> EventId {
        self.start[block as usize]
    }

    /// Returns the events in a block.
    pub fn events(&self, block: BlockId) -> Range<EventId> {
        self.start(block)..self.start(block + 1)
    }

    /// Returns the blocks that a block's first event has as parents.
    pub fn parents(&self, block: BlockId) -> &[BlockId] {
        let i = block as usize;
        &self.parents[self.parent_start[i] as usize..self.parent_start[i + 1] as usize]
    }

    /// Finds the block holding an event.
    pub fn containing(&self, event: EventId) -> BlockId {
        // Parents are mostly recent events, so the search widens backwards from the last block.
        let (mut end, mut step) = (self.len() as usize, 1);
        while step < end && self.start[end - step] > event {
            end -= step;
            step *= 2;
        }
        let from = end.saturating_sub(step);
        (from + self.start[from..end].partition_point(|&start| start <= event) - 1) as BlockId
    }
}
