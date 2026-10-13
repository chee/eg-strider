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
#[derive(Default)]
pub struct Graph {
    entries: Vec<Entry>,
    parents: Vec<u32>,
    heads: Vec<u32>,
}

/// Events `start..end`, where each after the first has the previous one as its only parent.
struct Entry {
    start: u32,
    end: u32,
    /// Range in `Graph::parents` holding the first event's parents.
    parents: (u32, u32),
}

impl Graph {
    /// Creates an empty graph.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the number of events.
    pub fn len(&self) -> u32 {
        self.entries.last().map_or(0, |entry| entry.end)
    }

    /// Tells whether the graph has no events.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
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
            self.entries.last_mut().expect("an entry to continue").end = end;
        } else {
            let at = self.parents.len() as u32;
            self.parents.extend_from_slice(parents);
            self.entries.push(Entry { start, end, parents: (at, self.parents.len() as u32) });
        }
        self.heads.retain(|h| !parents.contains(h));
        self.heads.push(end - 1);
        start..end
    }

    /// Cuts the graph into [`Blocks`].
    pub fn blocks(&self) -> Blocks {
        let n = self.len();

        // A block starts wherever an entry does, and just after every event that is some
        // other event's parent.
        let mut cut = vec![0u64; n as usize / 64 + 1];
        let mut mark = |event: u32| cut[event as usize / 64] |= 1 << (event % 64);
        for e in &self.entries {
            mark(e.start);
        }
        for &p in &self.parents {
            if p + 1 < n {
                mark(p + 1);
            }
        }

        let mut rank = Vec::with_capacity(cut.len());
        let mut count = 0u32;
        for w in &cut {
            rank.push(count);
            count += w.count_ones();
        }
        let mut start = Vec::with_capacity(count as usize + 1);
        for (i, &w) in cut.iter().enumerate() {
            let mut w = w;
            while w != 0 {
                start.push(i as u32 * 64 + w.trailing_zeros());
                w &= w - 1;
            }
        }
        start.push(n);

        // The block containing an event: one less than the number of starts at or before it.
        let block_of = |event: u32| {
            let below = cut[event as usize / 64] & (u64::MAX >> (63 - event % 64));
            rank[event as usize / 64] + below.count_ones() - 1
        };

        let mut parent_start = Vec::with_capacity(count as usize + 1);
        let mut parents = Vec::with_capacity(self.parents.len() + count as usize);
        let mut block = 0usize;
        for e in &self.entries {
            debug_assert_eq!(start[block], e.start);
            parent_start.push(parents.len() as u32);
            parents.extend(self.parents[e.parents.0 as usize..e.parents.1 as usize].iter().map(|&p| block_of(p)));
            block += 1;
            // Later pieces of the same entry each continue from the piece before.
            while start[block] < e.end {
                parent_start.push(parents.len() as u32);
                parents.push(block as u32 - 1);
                block += 1;
            }
        }
        parent_start.push(parents.len() as u32);

        Blocks { start, parent_start, parents }
    }
}

/// A graph cut into *blocks*: maximal chains of events that no other event branches from
/// part-way along. A version therefore contains a block entirely or not at all, which lets
/// versions be handled as sets of blocks.
pub struct Blocks {
    /// Run `i` covers events `start[i]..start[i + 1]`.
    start: Vec<u32>,
    parent_start: Vec<u32>,
    parents: Vec<u32>,
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

    /// Splits the history at its critical versions: the points where every later event has
    /// every earlier event in its history, so that nothing before the point can affect how
    /// anything after it is merged.
    pub fn segments(&self) -> Vec<Segment> {
        let blocks = self.len();
        let start = &self.start;

        // seen[i]: a `q` such that block i's first event has all of events 0..q in its
        // history. Exact when the block has seen everything before it (its parents are then
        // exactly the heads of the graph so far), otherwise a lower bound from its parents.
        let mut seen = vec![0u32; blocks];
        let mut is_head = vec![false; blocks];
        let mut heads = 0usize;
        for i in 0..blocks {
            let parents = self.parents(i);
            seen[i] = if parents.len() == heads && parents.iter().all(|&p| is_head[p as usize]) {
                start[i]
            } else {
                let seen_after = |p: usize| if seen[p] == start[p] { start[p + 1] } else { seen[p] };
                parents.iter().map(|&p| seen_after(p as usize)).max().unwrap_or(0)
            };
            for &p in parents {
                if std::mem::take(&mut is_head[p as usize]) {
                    heads -= 1;
                }
            }
            is_head[i] = true;
            heads += 1;
        }

        // The version just before block i is critical when every block from i on has seen all
        // of events 0..start[i].
        let mut least_seen = vec![u32::MAX; blocks + 1];
        for i in (0..blocks).rev() {
            least_seen[i] = least_seen[i + 1].min(seen[i]);
        }
        let critical = |i: usize| least_seen[i] >= start[i];

        let mut segments = Vec::new();
        let mut i = 0;
        while i < blocks {
            debug_assert!(critical(i));
            let mut end = i + 1;
            if critical(end) {
                // Critical on both sides: nothing is concurrent with this block. Take every
                // block that follows in the same way.
                while end < blocks && critical(end + 1) {
                    end += 1;
                }
                segments.push(Segment::Linear(i..end));
            } else {
                while !critical(end) {
                    end += 1;
                }
                segments.push(Segment::Region(i..end));
            }
            i = end;
        }
        segments
    }
}
