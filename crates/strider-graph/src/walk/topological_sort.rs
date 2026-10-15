use super::{ChainDecomposition, JaggedArray};
use crate::{EventRunIndex, EventRuns};
use std::ops::Range;

/// How many of the most recently readied runs to consider when none follows the last one
/// directly.
const CANDIDATES: usize = 64;

/// A topological sort of a concurrent range's runs, numbered from 0 within it: the edges
/// between them, and which can be visited next.
pub(super) struct TopologicalSort {
    pub(super) parents: JaggedArray,
    children: JaggedArray,
    /// Which 127ths of the range's events lie at or after each run: a cheap measure of its
    /// descendants that counts shared ones once.
    cover: Vec<u128>,
    /// Per run, how many parents are not yet visited. A run outside `first` also waits on all
    /// of `first`, as one more parent.
    unvisited_parents: Vec<u32>,
    /// Runs waiting on nothing, in the order they became so.
    ready: Vec<u32>,
    /// Runs to visit before any other, and how many of them are left.
    first: Range<usize>,
    first_left: usize,
}

impl TopologicalSort {
    /// Records the edges between the range's runs, and starts with the first `known` runs
    /// visited and the rest of the first `first` to visit before any other.
    pub(super) fn new(runs: &EventRuns, range: &Range<EventRunIndex>, known: usize, first: usize) -> Self {
        let count = range.len();
        let mut parents = JaggedArray::new();
        for run in range.start..range.end {
            let inside = runs.parents(run).iter().filter_map(|&p| p.checked_sub(range.start));
            parents.items.extend(inside);
            parents.end_row();
        }
        let edges = (0..count).flat_map(|run| parents.row(run).iter().map(move |&p| (p, run as u32)));
        let children = JaggedArray::grouped(count, edges);

        let (from, to) = (runs.start(range.start), runs.start(range.end));
        let per_bit = (to - from).div_ceil(127).max(1);
        let mut cover = vec![0u128; count];
        for run in (0..count).rev() {
            let events = runs.events(range.start + run as EventRunIndex);
            let low = (events.start - from) / per_bit;
            let high = (events.end - from).div_ceil(per_bit);
            let mut covered = (1u128 << high) - (1u128 << low);
            for &child in children.row(run) {
                covered |= cover[child as usize];
            }
            cover[run] = covered;
        }

        let first = known..first.max(known);
        let first_left = first.len();
        let (mut unvisited_parents, mut ready) = (Vec::with_capacity(count), Vec::new());
        for run in 0..count {
            let waiting = parents.row(run).iter().filter(|&&p| p as usize >= known).count() as u32 + u32::from(first_left > 0 && run >= first.end);
            unvisited_parents.push(waiting);
            if waiting == 0 && run >= known {
                ready.push(run as u32);
            }
        }
        TopologicalSort { parents, children, cover, unvisited_parents, ready, first, first_left }
    }

    /// Takes the ready run nearest version `current`, preferring a child of the `last` run
    /// visited, which moves the version nowhere.
    pub(super) fn take_nearest(&mut self, chains: &ChainDecomposition, current: &[u32], last: Option<u32>) -> Option<usize> {
        let mut best = (u64::MAX, u32::MAX, None);
        let oldest = self.ready.len().saturating_sub(CANDIDATES);
        for at in (oldest..self.ready.len()).rev() {
            let run = self.ready[at] as usize;
            let weight = self.cover[run].count_ones();
            // A run whose only parent is the last run visited sees exactly the current version.
            let cost = if last.is_some_and(|last| self.parents.row(run) == [last]) {
                0
            } else if best.0 == 0 {
                continue;
            } else {
                chains.distance(current, run, best.0)
            };
            if (cost, weight) < (best.0, best.1) {
                best = (cost, weight, Some(at));
            }
        }
        Some(self.ready.remove(best.2?) as usize)
    }

    /// Marks a run visited, readying the runs only waiting on it.
    pub(super) fn visited(&mut self, run: usize) {
        let last_of_first = self.first.contains(&run) && {
            self.first_left -= 1;
            self.first_left == 0
        };
        let rest = if last_of_first { self.first.end as u32..self.unvisited_parents.len() as u32 } else { 0..0 };
        for child in self.children.row(run).iter().copied().chain(rest) {
            self.unvisited_parents[child as usize] -= 1;
            if self.unvisited_parents[child as usize] == 0 {
                self.ready.push(child);
            }
        }
    }
}
