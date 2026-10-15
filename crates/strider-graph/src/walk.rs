use crate::{EventRunIndex, EventRuns};
use std::ops::Range;

/// A step of a walk: the paper's retreat, advance and apply, over event runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WalkStep {
    Retreat(Range<EventRunIndex>),
    Advance(Range<EventRunIndex>),
    Apply(EventRunIndex),
}

struct JaggedArray {
    start: Vec<u32>,
    items: Vec<u32>,
}

impl JaggedArray {
    /// An empty array whose one open row takes the items pushed onto `items`.
    fn new() -> Self {
        JaggedArray { start: vec![0], items: Vec::new() }
    }

    fn end_row(&mut self) {
        self.start.push(self.items.len() as u32);
    }

    /// The number of closed rows.
    fn len(&self) -> usize {
        self.start.len() - 1
    }

    fn row(&self, i: usize) -> &[u32] {
        &self.items[self.start[i] as usize..self.start[i + 1] as usize]
    }

    /// Groups `(row, item)` pairs into `rows` rows, stably.
    fn grouped(rows: usize, pairs: impl Iterator<Item = (u32, u32)> + Clone) -> Self {
        let mut start = vec![0; rows + 1];
        for (row, _) in pairs.clone() {
            start[row as usize + 1] += 1;
        }
        for i in 1..=rows {
            start[i] += start[i - 1];
        }
        let mut items = vec![0; start[rows] as usize];
        for (row, item) in pairs {
            items[start[row as usize] as usize] = item;
            start[row as usize] += 1;
        }
        // Each start has moved to its row's end, which is where the next row starts.
        start.rotate_right(1);
        start[0] = 0;
        JaggedArray { start, items }
    }
}

/// A decomposition of a concurrent range's runs into chains, so that any version is a prefix of
/// each chain.
struct ChainDecomposition {
    chain: Vec<u32>,
    /// Each run's index along its chain.
    place: Vec<u32>,
    /// Each chain's runs, in order.
    members: JaggedArray,
    /// Per chain, prefix sums of its runs' event counts.
    events_before: JaggedArray,
    /// Each run's history as a version vector: how many runs of each chain it holds, leaving off
    /// chains begun after it.
    version_vectors: JaggedArray,
}

impl ChainDecomposition {
    /// Greedily puts each run on the first chain it has seen all of, else on a new one.
    fn new(parents: &JaggedArray, runs: &EventRuns, range: &Range<EventRunIndex>) -> Self {
        let (mut chain, mut place, mut version_vectors) = (Vec::new(), Vec::new(), JaggedArray::new());
        let mut lens: Vec<u32> = Vec::new();
        for run in 0..range.len() {
            let from = version_vectors.items.len();
            version_vectors.items.resize(from + lens.len(), 0);
            let (earlier, row) = version_vectors.items.split_at_mut(from);
            for &p in parents.row(run) {
                let p = p as usize;
                let theirs = &earlier[version_vectors.start[p] as usize..version_vectors.start[p + 1] as usize];
                for (mine, &theirs) in row.iter_mut().zip(theirs) {
                    *mine = (*mine).max(theirs);
                }
                let own = &mut row[chain[p] as usize];
                *own = (*own).max(place[p] + 1);
            }
            let joined = row.iter().zip(&lens).position(|(seen, len)| seen == len).unwrap_or_else(|| {
                lens.push(0);
                version_vectors.items.push(0);
                lens.len() - 1
            });
            version_vectors.end_row();
            chain.push(joined as u32);
            place.push(lens[joined]);
            lens[joined] += 1;
        }

        let members = JaggedArray::grouped(lens.len(), chain.iter().copied().zip(0..));
        let mut events_before = JaggedArray::new();
        for of_chain in 0..members.len() {
            let mut events = 0;
            events_before.items.push(0);
            for &member in members.row(of_chain) {
                events += runs.events(range.start + member).len() as u32;
                events_before.items.push(events);
            }
            events_before.end_row();
        }
        ChainDecomposition { chain, place, members, events_before, version_vectors }
    }

    /// How many runs of `chain` are in `run`'s history.
    fn history_along(&self, run: usize, chain: usize) -> u32 {
        self.version_vectors.row(run).get(chain).copied().unwrap_or(0)
    }

    /// The runs at `places` along `chain`; none if `places` is reversed.
    fn between(&self, chain: usize, places: Range<u32>) -> &[u32] {
        self.members.row(chain).get(places.start as usize..places.end as usize).unwrap_or(&[])
    }

    /// The number of events in one of `current` and `run`'s history but not the other, counted
    /// until past `limit`.
    fn distance(&self, current: &[u32], run: usize, limit: u64) -> u64 {
        let mut events = 0;
        for (chain, &now) in current.iter().enumerate() {
            let before = self.events_before.row(chain);
            events += before[now as usize].abs_diff(before[self.history_along(run, chain) as usize]) as u64;
            if events > limit {
                break;
            }
        }
        events
    }

    /// The version holding the first `known` runs.
    fn start(&self, known: usize) -> Vec<u32> {
        let mut version = vec![0; self.members.len()];
        for run in 0..known {
            version[self.chain[run] as usize] = self.place[run] + 1;
        }
        version
    }

    /// Sets `current` to `run`'s history and `run`.
    fn arrive(&self, run: usize, current: &mut [u32]) {
        let row = self.version_vectors.row(run);
        current[..row.len()].copy_from_slice(row);
        current[row.len()..].fill(0);
        current[self.chain[run] as usize] = self.place[run] + 1;
    }
}

/// How many of the most recently readied runs are weighed for the next step.
const CANDIDATES: usize = 64;

/// Kahn's algorithm over a concurrent range's runs, numbered from 0 within the range.
struct TopologicalSort {
    parents: JaggedArray,
    children: JaggedArray,
    /// Per run, which 127ths of the range's events it and its descendants hold: a size estimate
    /// that counts shared descendants once.
    cover: Vec<u128>,
    /// Per run, its unvisited parents, plus one for runs after `first` until all of `first` is
    /// visited.
    unvisited_parents: Vec<u32>,
    /// Runs with no unvisited parents, in the order they became ready.
    ready: Vec<u32>,
    /// Runs to visit before any other; `first_left` of them remain.
    first: Range<usize>,
    first_left: usize,
}

impl TopologicalSort {
    /// Starts with the first `known` runs visited and the rest of the first `first` due before any
    /// other.
    fn new(runs: &EventRuns, range: &Range<EventRunIndex>, known: usize, first: usize) -> Self {
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

    /// Takes the ready run whose history is nearest `current`, then the one with the fewest
    /// descendants. A run whose only parent is `last` is at distance 0.
    fn take_nearest(&mut self, chains: &ChainDecomposition, current: &[u32], last: Option<u32>) -> Option<usize> {
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

    /// Marks a run visited, readying any run that waited only on it.
    fn visited(&mut self, run: usize) {
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

/// Walks a concurrent range's runs in a topological order that keeps the prepare version's moves
/// short, calling `visit` with each step.
///
/// The first `known` runs start in the prepare version, and the rest of the first `first` come
/// before any other.
pub fn walk(runs: &EventRuns, range: &Range<EventRunIndex>, known: usize, first: usize, mut visit: impl FnMut(WalkStep)) {
    let mut sort = TopologicalSort::new(runs, range, known, first);
    let chains = ChainDecomposition::new(&sort.parents, runs, range);
    // The prepare version: how far along each chain it reaches.
    let mut prepare_version = chains.start(known);

    let in_graph = |first: u32, last: u32| range.start + first..range.start + last + 1;
    let mut last = None;
    while let Some(run) = sort.take_nearest(&chains, &prepare_version, last) {
        for (chain, &now) in prepare_version.iter().enumerate() {
            let leaving = chains.between(chain, chains.history_along(run, chain)..now);
            each_consecutive_range(leaving, |first, last| visit(WalkStep::Retreat(in_graph(first, last))));
        }
        for (chain, &now) in prepare_version.iter().enumerate() {
            let returning = chains.between(chain, now..chains.history_along(run, chain));
            each_consecutive_range(returning, |first, last| visit(WalkStep::Advance(in_graph(first, last))));
        }
        visit(WalkStep::Apply(range.start + run as EventRunIndex));
        chains.arrive(run, &mut prepare_version);
        sort.visited(run);
        last = Some(run as u32);
    }
}

/// Calls `f` with the first and last of each stretch of consecutive runs.
fn each_consecutive_range(runs: &[u32], mut f: impl FnMut(u32, u32)) {
    let mut from = 0;
    for to in 1..=runs.len() {
        if to == runs.len() || runs[to] != runs[to - 1] + 1 {
            f(runs[from], runs[to - 1]);
            from = to;
        }
    }
}
