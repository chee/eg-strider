//! The event graph of a document's history: which events each event's author had seen.
//! Merging is only needed where history is concurrent, so a replay first has to find where
//! that is. This crate stores the graph run-length encoded, cuts it into blocks (the basic
//! blocks of a control-flow graph: chains entered only at the top and left only at the
//! bottom), and finds the critical versions that every path passes through. The stretch
//! between two of them is either linear or a region: a single-entry single-exit part of the
//! graph with concurrency inside.

mod walk;

pub use walk::{Move, Walker};

use std::ops::Range;

/// An append-only causal graph, run-length encoded.
/// An append-only event graph, kept cut into blocks and with its critical versions known.
#[derive(Default)]
pub struct Graph {
    blocks: Blocks,
    /// The events that nothing yet descends from, in ascending order.
    heads: Vec<u32>,
    /// For each block, a number of events from the start of history that are all in its
    /// history: exact when the block has seen everything before it, else a lower bound.
    seen: Vec<u32>,
    /// The blocks that start at a critical version, in order.
    critical: Vec<u32>,
    /// Reused while adding events: the blocks their parents end.
    ends: Vec<u32>,
}

impl Graph {
    /// Creates an empty graph.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the number of events.
    pub fn len(&self) -> u32 {
        self.blocks.start[self.blocks.len()]
    }

    /// Tells whether the graph has no events.
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Returns the events that nothing yet descends from, in ascending order.
    pub fn frontier(&self) -> &[u32] {
        // Kept in order: each new event is the highest so far, and goes on the end.
        &self.heads
    }

    /// Tells whether a new event with these parents would simply follow the last event.
    pub fn continues(&self, parents: &[u32]) -> bool {
        !self.is_empty() && parents == [self.len() - 1]
    }

    /// Returns the graph's blocks.
    pub fn blocks(&self) -> &Blocks {
        &self.blocks
    }

    /// Adds `len` events in a chain, the first with the given parents. Returns their
    /// numbers.
    pub fn push(&mut self, parents: &[u32], len: u32) -> Range<u32> {
        let start = self.len();
        let end = start.checked_add(len).expect("too many events");
        assert!(parents.iter().all(|&p| p < start), "parent is not a known event");
        // Parents usually arrive in order; only otherwise are they copied to be sorted.
        let sorted: Vec<u32>;
        let parents = if parents.windows(2).all(|pair| pair[0] < pair[1]) {
            parents
        } else {
            let mut copy = parents.to_vec();
            copy.sort_unstable();
            copy.dedup();
            sorted = copy;
            &sorted
        };

        if self.continues(parents) {
            // Nothing else can have the last event as a parent yet, so its block just grows.
            *self.blocks.start.last_mut().expect("a block to continue") = end;
        } else {
            // A parent part-way along a block splits it there. Taking them in ascending
            // order keeps the blocks already found where they were.
            let mut ends = std::mem::take(&mut self.ends);
            ends.clear();
            for &parent in parents {
                let block = self.blocks.containing(parent);
                if parent + 1 < self.blocks.start[block + 1] {
                    self.split(block, parent + 1);
                }
                ends.push(block as u32);
            }
            let seen = if parents == self.heads { start } else { ends.iter().map(|&block| self.seen_after(block as usize)).max().unwrap_or(0) };
            let new = self.blocks.len() as u32;
            self.blocks.parents.extend_from_slice(&ends);
            self.blocks.parent_start.push(self.blocks.parents.len() as u32);
            self.blocks.start.push(end);
            self.seen.push(seen);
            self.ends = ends;

            // A version is critical only while every later block has seen all that came
            // before it.
            while self.critical.last().is_some_and(|&block| self.blocks.start[block as usize] > seen) {
                self.critical.pop();
            }
            if seen == start {
                self.critical.push(new);
            }
        }
        self.heads.retain(|h| !parents.contains(h));
        self.heads.push(end - 1);
        start..end
    }

    /// Returns the arrays the graph is made of, for storing it with what it has worked out.
    pub fn parts(&self) -> Parts<&[u32]> {
        Parts {
            start: &self.blocks.start,
            parent_start: &self.blocks.parent_start,
            parents: &self.blocks.parents,
            heads: &self.heads,
            seen: &self.seen,
            critical: &self.critical,
        }
    }

    /// Puts a graph back together from the arrays of one, or returns `None` if they cannot be
    /// a graph's. What they say about the history is taken on trust.
    pub fn from_parts(parts: Parts<Vec<u32>>) -> Option<Self> {
        let Parts { start, parent_start, parents, heads, seen, critical } = parts;
        let blocks = start.len().checked_sub(1)?;
        let events = start[blocks];
        let ascending = |list: &[u32]| list.windows(2).all(|pair| pair[0] < pair[1]);
        let sound = start[0] == 0
            && ascending(&start)
            && parent_start.len() == start.len()
            && parent_start[0] == 0
            && parent_start.windows(2).all(|pair| pair[0] <= pair[1])
            && parent_start[blocks] as usize == parents.len()
            // A block's parents are blocks before it.
            && (0..blocks).all(|block| parents[parent_start[block] as usize..parent_start[block + 1] as usize].iter().all(|&parent| (parent as usize) < block))
            && seen.len() == blocks
            && seen.iter().zip(&start).all(|(seen, start)| seen <= start)
            && ascending(&critical)
            && critical.last().is_none_or(|&block| (block as usize) < blocks)
            && ascending(&heads)
            && heads.last().map_or(events == 0, |&head| head + 1 == events);
        sound.then(|| Graph { blocks: Blocks { start, parent_start, parents }, heads, seen, critical, ends: Vec::new() })
    }

    /// Returns how many events from the start of history are all in the history of a
    /// block's last event.
    fn seen_after(&self, block: usize) -> u32 {
        if self.seen[block] == self.blocks.start[block] {
            self.blocks.start[block + 1]
        } else {
            self.seen[block]
        }
    }

    /// Splits a block so that a new one starts at event `at`.
    fn split(&mut self, block: usize, at: u32) {
        let blocks = &mut self.blocks;
        let (first, second) = (block as u32, block as u32 + 1);
        // Later blocks named this one for its last event, which the second part now has;
        // and every block after it moves up one.
        let later = blocks.parent_start[block + 1] as usize;
        for parent in &mut blocks.parents[later..] {
            *parent += u32::from(*parent >= first);
        }
        blocks.parents.insert(later, first);
        blocks.start.insert(block + 1, at);
        blocks.parent_start.insert(block + 1, later as u32);
        for start in &mut blocks.parent_start[block + 2..] {
            *start += 1;
        }
        self.seen.insert(block + 1, if self.seen[block] == blocks.start[block] { at } else { self.seen[block] });

        let moved = self.critical.partition_point(|&critical| critical <= first);
        for critical in &mut self.critical[moved..] {
            *critical += 1;
        }
        if self.seen[block + 1..].iter().all(|&seen| seen >= at) {
            self.critical.insert(moved, second);
        }
    }

    /// Splits the history at its critical versions: the points where every later event has
    /// every earlier event in its history.
    pub fn segments(&self) -> Vec<Segment> {
        let blocks = self.blocks.len();
        let next = |i: usize| self.critical.get(i + 1).map_or(blocks, |&block| block as usize);
        let mut segments = Vec::new();
        let mut i = 0;
        while i < self.critical.len() {
            let from = self.critical[i] as usize;
            if next(i) == from + 1 {
                // Critical on both sides: nothing is concurrent with this block. Take
                // every block that follows in the same way.
                while i + 1 < self.critical.len() && next(i + 1) == self.critical[i + 1] as usize + 1 {
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

/// The arrays a [`Graph`] is made of, as [`Graph::parts`] hands them out and
/// [`Graph::from_parts`] takes them back.
pub struct Parts<T> {
    /// The first event of each block, then the number of events.
    pub start: T,
    /// Where each block's parents begin in `parents`, then how many there are in all.
    pub parent_start: T,
    /// Each block's parents: the blocks that its first event's parents end.
    pub parents: T,
    /// The events that nothing yet descends from.
    pub heads: T,
    /// For each block, how many events from the start of history are known to be in its history.
    pub seen: T,
    /// The blocks that start at a critical version.
    pub critical: T,
}

/// A graph cut into *blocks*: maximal chains of events that no other event branches from
/// part-way along. A version therefore contains a block entirely or not at all.
pub struct Blocks {
    /// Block `i` covers events `start[i]..start[i + 1]`.
    start: Vec<u32>,
    parent_start: Vec<u32>,
    /// Each block's parents: the blocks that its first event's parents end.
    parents: Vec<u32>,
}

impl Default for Blocks {
    fn default() -> Self {
        Blocks { start: vec![0], parent_start: vec![0], parents: Vec::new() }
    }
}

/// A stretch of history between two critical versions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Segment {
    /// Blocks that nothing is concurrent with. Their events mean exactly what they say.
    Linear(Range<usize>),
    /// Blocks with concurrency among them, which have to be merged.
    Region(Range<usize>),
}

impl Segment {
    /// Returns the blocks in the segment.
    pub fn blocks(&self) -> &Range<usize> {
        match self {
            Segment::Linear(blocks) | Segment::Region(blocks) => blocks,
        }
    }
}

impl Blocks {
    /// Returns the number of blocks.
    pub fn len(&self) -> usize {
        self.parent_start.len() - 1
    }

    /// Tells whether there are no blocks.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the first event of a block. `block` may be one past the last block, giving the total
    /// number of events.
    pub fn start(&self, block: usize) -> u32 {
        self.start[block]
    }

    /// Returns the events in a block.
    pub fn events(&self, block: usize) -> Range<u32> {
        self.start[block]..self.start[block + 1]
    }

    /// Returns the blocks that a block's first event has as parents.
    pub fn parents(&self, block: usize) -> &[u32] {
        &self.parents[self.parent_start[block] as usize..self.parent_start[block + 1] as usize]
    }

    /// Finds the block holding an event.
    fn containing(&self, event: u32) -> usize {
        // Parents are mostly recent events, so the search widens backwards from the last block.
        let (mut end, mut step) = (self.len(), 1);
        while step < end && self.start[end - step] > event {
            end -= step;
            step *= 2;
        }
        let from = end.saturating_sub(step);
        from + self.start[from..end].partition_point(|&start| start <= event) - 1
    }
}
