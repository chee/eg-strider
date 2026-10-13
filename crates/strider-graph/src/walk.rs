//! Plans the order in which a region's blocks are merged. The tracker can merge a block
//! only when its working version equals the block's parents, and moving that version costs
//! time in proportion to the events moved. The plan is a topological sort (Kahn's
//! algorithm) that takes, among the blocks that are ready, the one nearest the current
//! version. A version is held as a version vector: the region's blocks are split into
//! chains, and a version is how far along each chain it reaches.

use crate::Blocks;
use std::ops::Range;

/// One step of a walk. Ranges are of blocks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Move {
    /// Take these blocks out of the working version.
    Retreat(Range<usize>),
    /// Put these blocks back into the working version.
    Advance(Range<usize>),
    /// The working version is now this block's parents: merge the block, which also adds it to
    /// the working version.
    Apply(usize),
}

/// A list of lists stored flat: row `i` is `items[start[i]..start[i + 1]]`.
#[derive(Default)]
struct Rows {
    start: Vec<u32>,
    items: Vec<u32>,
}

impl Rows {
    /// Empties the list, leaving one open row to push to.
    fn clear(&mut self) {
        self.start.clear();
        self.start.push(0);
        self.items.clear();
    }

    /// Closes the open row and opens the next.
    fn end_row(&mut self) {
        self.start.push(self.items.len() as u32);
    }

    /// Returns how many closed rows there are.
    fn len(&self) -> usize {
        self.start.len() - 1
    }

    fn row(&self, i: usize) -> &[u32] {
        &self.items[self.start[i] as usize..self.start[i + 1] as usize]
    }

    /// Rebuilds the list as `rows` rows from `(row, item)` pairs, keeping the order of the
    /// items within each row.
    fn group(&mut self, rows: usize, pairs: impl Iterator<Item = (u32, u32)> + Clone) {
        self.start.clear();
        self.start.resize(rows + 1, 0);
        for (row, _) in pairs.clone() {
            self.start[row as usize + 1] += 1;
        }
        for i in 1..=rows {
            self.start[i] += self.start[i - 1];
        }
        self.items.clear();
        self.items.resize(self.start[rows] as usize, 0);
        for (row, item) in pairs {
            self.items[self.start[row as usize] as usize] = item;
            self.start[row as usize] += 1;
        }
        // Each start has moved to its row's end, which is where the next row starts.
        self.start.rotate_right(1);
        self.start[0] = 0;
    }
}

/// The edges between a region's blocks, which are numbered from 0 within it.
#[derive(Default)]
struct RegionGraph {
    parents: Rows,
    children: Rows,
}

impl RegionGraph {
    fn build(&mut self, blocks: &Blocks, region: &Range<usize>) {
        let RegionGraph { parents, children } = self;
        parents.clear();
        for block in region.start..region.end {
            let inside = blocks.parents(block).iter().filter_map(|&p| (p as usize).checked_sub(region.start));
            parents.items.extend(inside.map(|p| p as u32));
            parents.end_row();
        }
        let edges = (0..region.len()).flat_map(|block| parents.row(block).iter().map(move |&p| (p, block as u32)));
        children.group(region.len(), edges);
    }
}

/// A region's blocks split into chains, in which every block has the one before it in its
/// history. A history then holds a prefix of each chain, and is stored as those lengths.
#[derive(Default)]
struct Chains {
    /// The chain each block is on, and how many blocks come before it there.
    chain: Vec<u32>,
    place: Vec<u32>,
    /// Each chain's blocks, in order.
    members: Rows,
    /// For each chain, the number of events in its first `j` blocks, for every `j`.
    events_before: Rows,
    /// Each block's history: how many blocks along each chain are in it. Chains that began
    /// after the block are left off.
    seen: Rows,
}

impl Chains {
    /// Splits the blocks into chains and works out each block's history. A block joins a
    /// chain it has seen the whole of, if there is one.
    fn build(&mut self, graph: &RegionGraph, blocks: &Blocks, region: &Range<usize>) {
        let Chains { chain, place, members, events_before, seen } = self;
        chain.clear();
        place.clear();
        seen.clear();
        let mut lens: Vec<u32> = Vec::new();
        for block in 0..region.len() {
            let from = seen.items.len();
            seen.items.resize(from + lens.len(), 0);
            let (earlier, row) = seen.items.split_at_mut(from);
            for &p in graph.parents.row(block) {
                let p = p as usize;
                let theirs = &earlier[seen.start[p] as usize..seen.start[p + 1] as usize];
                for (mine, &theirs) in row.iter_mut().zip(theirs) {
                    *mine = (*mine).max(theirs);
                }
                let own = &mut row[chain[p] as usize];
                *own = (*own).max(place[p] + 1);
            }
            let joined = row.iter().zip(&lens).position(|(seen, len)| seen == len).unwrap_or_else(|| {
                lens.push(0);
                seen.items.push(0);
                lens.len() - 1
            });
            seen.end_row();
            chain.push(joined as u32);
            place.push(lens[joined]);
            lens[joined] += 1;
        }

        members.group(lens.len(), chain.iter().copied().zip(0..));
        events_before.clear();
        for of_chain in 0..members.len() {
            let mut events = 0;
            events_before.items.push(0);
            for &member in members.row(of_chain) {
                events += blocks.events(region.start + member as usize).len() as u32;
                events_before.items.push(events);
            }
            events_before.end_row();
        }
    }

    /// Returns how many blocks along `chain` are in a block's history.
    fn seen(&self, block: usize, chain: usize) -> u32 {
        self.seen.row(block).get(chain).copied().unwrap_or(0)
    }

    /// Returns the blocks at `places` along a chain, or none if `places` runs backwards.
    fn between(&self, chain: usize, places: Range<u32>) -> &[u32] {
        self.members.row(chain).get(places.start as usize..places.end as usize).unwrap_or(&[])
    }

    /// Counts the events between the version `current` and a block's history, giving up once
    /// past `limit`.
    fn distance(&self, current: &[u32], block: usize, limit: u64) -> u64 {
        let mut events = 0;
        for (chain, &now) in current.iter().enumerate() {
            let before = self.events_before.row(chain);
            events += before[now as usize].abs_diff(before[self.seen(block, chain) as usize]) as u64;
            if events > limit {
                break;
            }
        }
        events
    }

    /// Sets `current` to the version holding the first `known` blocks.
    fn start(&self, known: usize, current: &mut Vec<u32>) {
        current.clear();
        current.resize(self.members.len(), 0);
        for block in 0..known {
            current[self.chain[block] as usize] = self.place[block] + 1;
        }
    }

    /// Sets `current` to a block's history plus the block itself.
    fn arrive(&self, block: usize, current: &mut [u32]) {
        let row = self.seen.row(block);
        current[..row.len()].copy_from_slice(row);
        current[row.len()..].fill(0);
        current[self.chain[block] as usize] = self.place[block] + 1;
    }
}

/// When no block continues straight on from the last one, how many of the most recently
/// readied blocks to consider as the next stop.
const CANDIDATES: usize = 64;

/// The state of the topological sort: which blocks can be visited next.
#[derive(Default)]
struct Schedule {
    /// Which 127ths of the region's events lie at or after each block: a cheap stand-in for
    /// the size of its set of descendants that does not count shared descendants twice.
    cover: Vec<u128>,
    /// How many of each block's parents have not been visited yet.
    waiting: Vec<u32>,
    /// The blocks waiting on nothing, in the order they became so.
    ready: Vec<u32>,
}

impl Schedule {
    /// Starts with the first `known` blocks counted as visited.
    fn build(&mut self, graph: &RegionGraph, blocks: &Blocks, region: &Range<usize>, known: usize) {
        let count = region.len();
        let (from, to) = (blocks.start(region.start), blocks.start(region.end));
        let per_bit = (to - from).div_ceil(127).max(1);
        self.cover.clear();
        self.cover.resize(count, 0);
        for block in (0..count).rev() {
            let events = blocks.events(region.start + block);
            let low = (events.start - from) / per_bit;
            let high = (events.end - from).div_ceil(per_bit);
            let mut cover = (1u128 << high) - (1u128 << low);
            for &child in graph.children.row(block) {
                cover |= self.cover[child as usize];
            }
            self.cover[block] = cover;
        }

        self.waiting.clear();
        self.ready.clear();
        for block in 0..count {
            let waiting = graph.parents.row(block).iter().filter(|&&p| p as usize >= known).count() as u32;
            self.waiting.push(waiting);
            if waiting == 0 && block >= known {
                self.ready.push(block as u32);
            }
        }
    }

    /// Takes the ready block nearest the version `current`: a child of the `last` block
    /// visited when one is ready, since that moves the version nowhere.
    fn take_nearest(&mut self, graph: &RegionGraph, chains: &Chains, current: &[u32], last: Option<u32>) -> Option<usize> {
        let mut best = (u64::MAX, u32::MAX, None);
        let oldest = self.ready.len().saturating_sub(CANDIDATES);
        for at in (oldest..self.ready.len()).rev() {
            let block = self.ready[at] as usize;
            let weight = self.cover[block].count_ones();
            // A block whose only parent is the last block visited sees exactly the current
            // version.
            let cost = if last.is_some_and(|last| graph.parents.row(block) == [last]) {
                0
            } else if best.0 == 0 {
                continue;
            } else {
                chains.distance(current, block, best.0)
            };
            if (cost, weight) < (best.0, best.1) {
                best = (cost, weight, Some(at));
            }
        }
        Some(self.ready.remove(best.2?) as usize)
    }

    /// Marks a block visited, readying any children that were only waiting on it.
    fn visited(&mut self, graph: &RegionGraph, block: usize) {
        for &child in graph.children.row(block) {
            self.waiting[child as usize] -= 1;
            if self.waiting[child as usize] == 0 {
                self.ready.push(child);
            }
        }
    }
}

/// Plans walks. Holds scratch space that is reused from one region to the next.
#[derive(Default)]
pub struct Walker {
    graph: RegionGraph,
    chains: Chains,
    schedule: Schedule,
    /// The working version: how far along each chain it reaches.
    current: Vec<u32>,
}

impl Walker {
    /// Creates a walker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Walks the blocks in `region`, which must lie between two critical versions, calling
    /// `visit` with each step. The first `known` blocks are already in the working version.
    pub fn walk(&mut self, blocks: &Blocks, region: &Range<usize>, known: usize, mut visit: impl FnMut(Move)) {
        let Walker { graph, chains, schedule, current } = self;
        graph.build(blocks, region);
        chains.build(graph, blocks, region);
        schedule.build(graph, blocks, region, known);
        chains.start(known, current);

        let stretch = |first: u32, last: u32| region.start + first as usize..region.start + last as usize + 1;
        let mut last = None;
        while let Some(block) = schedule.take_nearest(graph, chains, current, last) {
            for (chain, &now) in current.iter().enumerate() {
                let leaving = chains.between(chain, chains.seen(block, chain)..now);
                each_stretch(leaving, |first, last| visit(Move::Retreat(stretch(first, last))));
            }
            for (chain, &now) in current.iter().enumerate() {
                let returning = chains.between(chain, now..chains.seen(block, chain));
                each_stretch(returning, |first, last| visit(Move::Advance(stretch(first, last))));
            }
            visit(Move::Apply(region.start + block));
            chains.arrive(block, current);
            schedule.visited(graph, block);
            last = Some(block as u32);
        }
    }
}

/// Calls `f` with the first and last of each stretch of blocks numbered one after another.
fn each_stretch(blocks: &[u32], mut f: impl FnMut(u32, u32)) {
    let mut from = 0;
    for to in 1..=blocks.len() {
        if to == blocks.len() || blocks[to] != blocks[to - 1] + 1 {
            f(blocks[from], blocks[to - 1]);
            from = to;
        }
    }
}
