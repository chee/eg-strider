mod chain_decomposition;
mod jagged_array;
mod topological_sort;

use crate::{EventRunIndex, EventRuns};
use chain_decomposition::ChainDecomposition;
use jagged_array::JaggedArray;
use std::ops::Range;
use topological_sort::TopologicalSort;

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

/// Walks a concurrent range, which lies between two critical versions, calling `visit` with
/// each step: the order its runs are merged in. Moving the prepare version costs time per event
/// moved, so a topological sort takes, among the ready runs, the one nearest the current
/// version, held as how far along each chain of runs it reaches. The first `known` runs start
/// in the prepare version, and the rest of the first `first` are visited before any other.
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
