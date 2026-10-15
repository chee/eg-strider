use crate::{EventRunIndex, EventRuns};
use std::ops::Range;

/// One step of a walk over event runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WalkStep {
    /// Take these runs out of the prepare version.
    Retreat(Range<EventRunIndex>),
    /// Put these runs back into the prepare version.
    Advance(Range<EventRunIndex>),
    /// Merge this run, whose parents are now the prepare version. It then joins the prepare
    /// version.
    Apply(EventRunIndex),
}

/// Lists of lists stored flat: row `i` is `items[start[i]..start[i + 1]]`.
#[derive(Default)]
struct JaggedArray {
    start: Vec<u32>,
    items: Vec<u32>,
}

impl JaggedArray {
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

    /// Returns the number of closed rows.
    fn len(&self) -> usize {
        self.start.len() - 1
    }

    /// Returns row `i`.
    fn row(&self, i: usize) -> &[u32] {
        &self.items[self.start[i] as usize..self.start[i + 1] as usize]
    }

    /// Rebuilds the list as `rows` rows from `(row, item)` pairs, keeping each row's items in
    /// order.
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

/// A concurrent range's runs split into chains, each run having the one before it in its
/// history. A version is then a prefix of each chain.
#[derive(Default)]
struct ChainDecomposition {
    /// Each run's chain, and its place along it.
    chain: Vec<u32>,
    place: Vec<u32>,
    /// Each chain's runs, in order.
    members: JaggedArray,
    /// Per chain, the events in its first `j` runs, for every `j`.
    events_before: JaggedArray,
    /// Each run's history as a version vector: how many runs along each chain it holds. Chains
    /// begun after the run are left off.
    version_vectors: JaggedArray,
}

impl ChainDecomposition {
    /// Splits the runs into chains and works out each run's history. A run joins a chain it has
    /// seen all of, if any.
    fn build(&mut self, parents: &JaggedArray, runs: &EventRuns, range: &Range<EventRunIndex>) {
        let ChainDecomposition { chain, place, members, events_before, version_vectors } = self;
        chain.clear();
        place.clear();
        version_vectors.clear();
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

        members.group(lens.len(), chain.iter().copied().zip(0..));
        events_before.clear();
        for of_chain in 0..members.len() {
            let mut events = 0;
            events_before.items.push(0);
            for &member in members.row(of_chain) {
                events += runs.events(range.start + member).len() as u32;
                events_before.items.push(events);
            }
            events_before.end_row();
        }
    }

    /// Returns how many runs along `chain` are in a run's history.
    fn history_along(&self, run: usize, chain: usize) -> u32 {
        self.version_vectors.row(run).get(chain).copied().unwrap_or(0)
    }

    /// Returns the runs at `places` along a chain, or none if `places` runs backwards.
    fn between(&self, chain: usize, places: Range<u32>) -> &[u32] {
        self.members.row(chain).get(places.start as usize..places.end as usize).unwrap_or(&[])
    }

    /// Counts the events between version `current` and a run's history, stopping once past
    /// `limit`.
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

    /// Sets `current` to the version holding the first `known` runs.
    fn start(&self, known: usize, current: &mut Vec<u32>) {
        current.clear();
        current.resize(self.members.len(), 0);
        for run in 0..known {
            current[self.chain[run] as usize] = self.place[run] + 1;
        }
    }

    /// Sets `current` to a run's history plus the run itself.
    fn arrive(&self, run: usize, current: &mut [u32]) {
        let row = self.version_vectors.row(run);
        current[..row.len()].copy_from_slice(row);
        current[row.len()..].fill(0);
        current[self.chain[run] as usize] = self.place[run] + 1;
    }
}

/// How many of the most recently readied runs to consider when none follows the last one
/// directly.
const CANDIDATES: usize = 64;

/// A topological sort of a concurrent range's runs, numbered from 0 within it: the edges
/// between them, and which can be visited next.
#[derive(Default)]
struct TopologicalSort {
    parents: JaggedArray,
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
    /// Records the edges between the range's runs.
    fn build_edges(&mut self, runs: &EventRuns, range: &Range<EventRunIndex>) {
        let TopologicalSort { parents, children, .. } = self;
        parents.clear();
        for run in range.start..range.end {
            let inside = runs.parents(run).iter().filter_map(|&p| p.checked_sub(range.start));
            parents.items.extend(inside);
            parents.end_row();
        }
        let edges = (0..range.len()).flat_map(|run| parents.row(run).iter().map(move |&p| (p, run as u32)));
        children.group(range.len(), edges);
    }

    /// Starts with the first `known` runs visited, and the rest of the first `first` to visit
    /// before any other.
    fn start(&mut self, runs: &EventRuns, range: &Range<EventRunIndex>, known: usize, first: usize) {
        let count = range.len();
        let (from, to) = (runs.start(range.start), runs.start(range.end));
        let per_bit = (to - from).div_ceil(127).max(1);
        self.cover.clear();
        self.cover.resize(count, 0);
        for run in (0..count).rev() {
            let events = runs.events(range.start + run as EventRunIndex);
            let low = (events.start - from) / per_bit;
            let high = (events.end - from).div_ceil(per_bit);
            let mut cover = (1u128 << high) - (1u128 << low);
            for &child in self.children.row(run) {
                cover |= self.cover[child as usize];
            }
            self.cover[run] = cover;
        }

        self.unvisited_parents.clear();
        self.ready.clear();
        self.first = known..first.max(known);
        self.first_left = self.first.len();
        for run in 0..count {
            let parents = self.parents.row(run).iter().filter(|&&p| p as usize >= known).count() as u32;
            let waiting = parents + u32::from(self.first_left > 0 && run >= self.first.end);
            self.unvisited_parents.push(waiting);
            if waiting == 0 && run >= known {
                self.ready.push(run as u32);
            }
        }
    }

    /// Takes the ready run nearest version `current`, preferring a child of the `last` run
    /// visited, which moves the version nowhere.
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

    /// Marks a run visited, readying the runs only waiting on it.
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

/// Space that walks reuse, so that walking many concurrent ranges allocates little.
#[derive(Default)]
pub struct WalkScratch {
    chains: ChainDecomposition,
    sort: TopologicalSort,
    /// The prepare version: how far along each chain it reaches.
    prepare_version: Vec<u32>,
}

/// Walks a concurrent range, which lies between two critical versions, calling `visit` with
/// each step: the order its runs are merged in. Moving the prepare version costs time per event
/// moved, so a topological sort takes, among the ready runs, the one nearest the current
/// version, held as how far along each chain of runs it reaches. The first `known` runs start
/// in the prepare version, and the rest of the first `first` are visited before any other.
pub fn walk(scratch: &mut WalkScratch, runs: &EventRuns, range: &Range<EventRunIndex>, known: usize, first: usize, mut visit: impl FnMut(WalkStep)) {
    let WalkScratch { chains, sort, prepare_version } = scratch;
    sort.build_edges(runs, range);
    chains.build(&sort.parents, runs, range);
    sort.start(runs, range, known, first);
    chains.start(known, prepare_version);

    let in_graph = |first: u32, last: u32| range.start + first..range.start + last + 1;
    let mut last = None;
    while let Some(run) = sort.take_nearest(chains, prepare_version, last) {
        for (chain, &now) in prepare_version.iter().enumerate() {
            let leaving = chains.between(chain, chains.history_along(run, chain)..now);
            each_consecutive_range(leaving, |first, last| visit(WalkStep::Retreat(in_graph(first, last))));
        }
        for (chain, &now) in prepare_version.iter().enumerate() {
            let returning = chains.between(chain, now..chains.history_along(run, chain));
            each_consecutive_range(returning, |first, last| visit(WalkStep::Advance(in_graph(first, last))));
        }
        visit(WalkStep::Apply(range.start + run as EventRunIndex));
        chains.arrive(run, prepare_version);
        sort.visited(run);
        last = Some(run as u32);
    }
}

/// Calls `f` with the first and last of each range of consecutive runs.
fn each_consecutive_range(runs: &[u32], mut f: impl FnMut(u32, u32)) {
    let mut from = 0;
    for to in 1..=runs.len() {
        if to == runs.len() || runs[to] != runs[to - 1] + 1 {
            f(runs[from], runs[to - 1]);
            from = to;
        }
    }
}
