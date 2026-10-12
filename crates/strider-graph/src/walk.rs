//! Planning a walk through a zone.

use crate::Runs;
use std::collections::BinaryHeap;
use std::ops::Range;

/// One step of a walk. Ranges are of events.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Move {
    /// Take these events out of the working version.
    Retreat(Range<u32>),
    /// Put these events back into the working version.
    Advance(Range<u32>),
    /// The working version is now this run's parents: merge the run, which also adds it to
    /// the working version.
    Apply(usize),
}

/// Plans walks. Holds scratch space that is reused from one zone to the next.
pub struct Walker {
    // The zone's runs are numbered locally from 0. Parents inside the zone, then the same
    // edges reversed.
    parent_start: Vec<u32>,
    parents: Vec<u32>,
    child_start: Vec<u32>,
    children: Vec<u32>,
    /// Which 127ths of the zone's events lie at or after each run: a cheap stand-in for the
    /// size of its set of descendants that does not count shared descendants twice.
    cover: Vec<u128>,
    waiting: Vec<u32>,
    ready: Vec<u32>,
    order: Vec<u32>,
    /// Row `i`: the set of runs in run `i`'s history.
    ancestors: Vec<u64>,
    current: Vec<u64>,
    bitset_limit: usize,
}

/// When no run continues straight on from the last one, how many of the most recently
/// readied runs to consider as the next stop.
const CANDIDATES: usize = 64;

impl Default for Walker {
    fn default() -> Self {
        Self::new()
    }
}

impl Walker {
    pub fn new() -> Self {
        // 16384 runs need 32 MB of ancestor sets.
        Self::with_bitset_limit(16384)
    }

    /// Zones of up to `runs` runs are planned with each run's full set of ancestors held as
    /// a bitset, which takes `runs² / 8` bytes.
    pub fn with_bitset_limit(runs: usize) -> Self {
        Walker {
            parent_start: Vec::new(),
            parents: Vec::new(),
            child_start: Vec::new(),
            children: Vec::new(),
            cover: Vec::new(),
            waiting: Vec::new(),
            ready: Vec::new(),
            order: Vec::new(),
            ancestors: Vec::new(),
            current: Vec::new(),
            bitset_limit: runs,
        }
    }

    /// Walks the runs in `zone`, which must lie between two critical versions, calling
    /// `visit` with each step.
    pub fn walk(&mut self, runs: &Runs, zone: &Range<usize>, known: usize, mut visit: impl FnMut(Move)) {
        let count = zone.len();
        self.build(runs, zone, known);
        let events = |run: u32| runs.events(zone.start + run as usize);

        if count <= self.bitset_limit {
            let words = count.div_ceil(64);
            self.build_ancestors(count, words);
            self.order_by_nearest(runs, zone, words, known);

            self.start_version(words, known);
            for &run in &self.order {
                let target = &self.ancestors[run as usize * words..][..words];
                for (i, (&now, &want)) in self.current.iter().zip(target).enumerate() {
                    each_bit(now & !want, |bit| visit(Move::Retreat(events(i as u32 * 64 + bit))));
                }
                for (i, (&now, &want)) in self.current.iter().zip(target).enumerate() {
                    each_bit(want & !now, |bit| visit(Move::Advance(events(i as u32 * 64 + bit))));
                }
                visit(Move::Apply(zone.start + run as usize));
                self.current.copy_from_slice(target);
                self.current[run as usize / 64] |= 1 << (run % 64);
            }
        } else {
            self.order_depth_first();
            // The working version, as the runs in it that have no children in it.
            let mut version: Vec<u32> = (0..known as u32)
                .filter(|&run| self.children(run as usize).iter().all(|&c| c as usize >= known))
                .collect();
            let (mut out, mut back) = (Vec::new(), Vec::new());
            for i in 0..self.order.len() {
                let run = self.order[i];
                self.difference(&version, self.parents(run as usize), &mut out, &mut back);
                out.iter().for_each(|&r| visit(Move::Retreat(events(r))));
                back.iter().rev().for_each(|&r| visit(Move::Advance(events(r))));
                visit(Move::Apply(zone.start + run as usize));
                version.clear();
                version.push(run);
            }
        }
    }

    fn parents(&self, run: usize) -> &[u32] {
        &self.parents[self.parent_start[run] as usize..self.parent_start[run + 1] as usize]
    }

    fn children(&self, run: usize) -> &[u32] {
        &self.children[self.child_start[run] as usize..self.child_start[run + 1] as usize]
    }

    /// Builds the zone's own graph, each run's descendant coverage, and the set of runs
    /// that are ready to be visited: those whose parents in the zone are all known.
    fn build(&mut self, runs: &Runs, zone: &Range<usize>, known: usize) {
        let count = zone.len();
        self.parent_start.clear();
        self.parents.clear();
        self.child_start.clear();
        self.child_start.resize(count + 2, 0);
        for run in zone.start..zone.end {
            self.parent_start.push(self.parents.len() as u32);
            for &p in runs.parents(run) {
                if p as usize >= zone.start {
                    let p = p - zone.start as u32;
                    self.parents.push(p);
                    self.child_start[p as usize + 2] += 1;
                }
            }
        }
        self.parent_start.push(self.parents.len() as u32);

        // child_start[i + 1] serves as the fill position for run i, which leaves
        // child_start[i] as the start of its children once every edge is placed.
        for i in 2..count + 2 {
            self.child_start[i] += self.child_start[i - 1];
        }
        self.children.clear();
        self.children.resize(self.parents.len(), 0);
        for run in 0..count {
            for k in self.parent_start[run]..self.parent_start[run + 1] {
                let p = self.parents[k as usize] as usize;
                self.children[self.child_start[p + 1] as usize] = run as u32;
                self.child_start[p + 1] += 1;
            }
        }

        let (from, to) = (runs.start(zone.start), runs.start(zone.end));
        let per_bit = (to - from).div_ceil(127).max(1);
        self.cover.clear();
        self.cover.resize(count, 0);
        for run in (0..count).rev() {
            let events = runs.events(zone.start + run);
            let low = (events.start - from) / per_bit;
            let high = (events.end - from).div_ceil(per_bit);
            let mut cover = (1u128 << high) - (1u128 << low);
            for &c in self.children(run) {
                cover |= self.cover[c as usize];
            }
            self.cover[run] = cover;
        }

        self.waiting.clear();
        self.ready.clear();
        self.order.clear();
        for run in 0..count {
            let waiting = self.parents(run).iter().filter(|&&p| p as usize >= known).count() as u32;
            self.waiting.push(waiting);
            if waiting == 0 && run >= known {
                self.ready.push(run as u32);
            }
        }
    }

    fn build_ancestors(&mut self, count: usize, words: usize) {
        self.ancestors.clear();
        self.ancestors.resize(count * words, 0);
        for run in 0..count {
            let (before, row) = self.ancestors.split_at_mut(run * words);
            let row = &mut row[..words];
            for &p in &self.parents[self.parent_start[run] as usize..self.parent_start[run + 1] as usize] {
                let p = p as usize;
                for (r, a) in row.iter_mut().zip(&before[p * words..][..words]) {
                    *r |= a;
                }
                row[p / 64] |= 1 << (p % 64);
            }
        }
    }

    /// Sets the current version to the first `known` runs.
    fn start_version(&mut self, words: usize, known: usize) {
        self.current.clear();
        self.current.resize(words, 0);
        for run in 0..known {
            self.current[run / 64] |= 1 << (run % 64);
        }
    }

    /// Adds `run` to the order and readies any children that were only waiting on it.
    fn visit(&mut self, run: u32) {
        self.order.push(run);
        for k in self.child_start[run as usize]..self.child_start[run as usize + 1] {
            let child = self.children[k as usize] as usize;
            self.waiting[child] -= 1;
            if self.waiting[child] == 0 {
                self.ready.push(child as u32);
            }
        }
    }

    /// Chooses the order to visit runs in. Carry straight on to a child of the last run
    /// when one is ready, since that moves the working version nowhere.
    fn order_by_nearest(&mut self, runs: &Runs, zone: &Range<usize>, words: usize, known: usize) {
        let events = |run: usize| runs.events(zone.start + run).len() as u64;
        self.start_version(words, known);

        while !self.ready.is_empty() {
            let last = self.order.last().copied();
            let mut best = (u64::MAX, u32::MAX, 0usize);
            let oldest = self.ready.len().saturating_sub(CANDIDATES);
            for at in (oldest..self.ready.len()).rev() {
                let run = self.ready[at] as usize;
                let weight = self.cover[run].count_ones();
                // A run whose only parent is the last run visited sees exactly the current
                // version.
                let cost = if last.is_some_and(|last| self.parents(run) == [last]) {
                    0
                } else if best.0 == 0 {
                    continue;
                } else {
                    let target = &self.ancestors[run * words..][..words];
                    let mut cost = 0;
                    'count: for (i, (&now, &want)) in self.current.iter().zip(target).enumerate() {
                        let mut differ = now ^ want;
                        while differ != 0 {
                            cost += events(i * 64 + differ.trailing_zeros() as usize);
                            if cost > best.0 {
                                break 'count;
                            }
                            differ &= differ - 1;
                        }
                    }
                    cost
                };
                if (cost, weight) < (best.0, best.1) {
                    best = (cost, weight, at);
                }
            }

            let run = self.ready.remove(best.2);
            self.visit(run);
            self.current.copy_from_slice(&self.ancestors[run as usize * words..][..words]);
            self.current[run as usize / 64] |= 1 << (run % 64);
        }
    }

    /// The order for zones too large for ancestor sets: depth first, always taking the most
    /// recently readied run, lighter branches before heavier ones.
    fn order_depth_first(&mut self) {
        let lightest_last = |cover: &[u128], ready: &mut [u32]| {
            ready.sort_unstable_by_key(|&run| std::cmp::Reverse(cover[run as usize].count_ones()));
        };
        lightest_last(&self.cover, &mut self.ready);
        while let Some(run) = self.ready.pop() {
            let before = self.ready.len();
            self.visit(run);
            lightest_last(&self.cover, &mut self.ready[before..]);
        }
    }

    /// The runs in the history of version `old` but not of `new`, and of `new` but not of
    /// `old`, both highest first. Found by walking back from both until only shared history
    /// remains.
    fn difference(&self, old: &[u32], new: &[u32], only_old: &mut Vec<u32>, only_new: &mut Vec<u32>) {
        const OLD: u8 = 0;
        const NEW: u8 = 1;
        const BOTH: u8 = 2;
        only_old.clear();
        only_new.clear();
        let mut heap: BinaryHeap<(u32, u8)> = BinaryHeap::new();
        heap.extend(old.iter().map(|&run| (run, OLD)));
        heap.extend(new.iter().map(|&run| (run, NEW)));
        let mut shared = 0usize;

        // Runs come off the heap highest first, so by the time one is taken everything that
        // descends from it has already left its mark.
        while let Some((run, mut side)) = heap.pop() {
            if side == BOTH {
                shared -= 1;
            }
            while let Some(&(next, other)) = heap.peek() {
                if next != run {
                    break;
                }
                heap.pop();
                if other == BOTH {
                    shared -= 1;
                }
                if other != side {
                    side = BOTH;
                }
            }
            match side {
                OLD => only_old.push(run),
                NEW => only_new.push(run),
                _ => {}
            }
            for &p in self.parents(run as usize) {
                heap.push((p, side));
                if side == BOTH {
                    shared += 1;
                }
            }
            if heap.len() == shared {
                break;
            }
        }
    }
}

fn each_bit(mut bits: u64, mut f: impl FnMut(u32)) {
    while bits != 0 {
        f(bits.trailing_zeros());
        bits &= bits - 1;
    }
}
