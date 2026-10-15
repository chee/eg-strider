use super::JaggedArray;
use crate::{EventRunIndex, EventRuns};
use std::ops::Range;

/// A concurrent range's runs split into chains, each run having the one before it in its
/// history. A version is then a prefix of each chain.
pub(super) struct ChainDecomposition {
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
    pub(super) fn new(parents: &JaggedArray, runs: &EventRuns, range: &Range<EventRunIndex>) -> Self {
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

    /// Returns how many runs along `chain` are in a run's history.
    pub(super) fn history_along(&self, run: usize, chain: usize) -> u32 {
        self.version_vectors.row(run).get(chain).copied().unwrap_or(0)
    }

    /// Returns the runs at `places` along a chain, or none if `places` runs backwards.
    pub(super) fn between(&self, chain: usize, places: Range<u32>) -> &[u32] {
        self.members.row(chain).get(places.start as usize..places.end as usize).unwrap_or(&[])
    }

    /// Counts the events between version `current` and a run's history, stopping once past
    /// `limit`.
    pub(super) fn distance(&self, current: &[u32], run: usize, limit: u64) -> u64 {
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

    /// Returns the version holding the first `known` runs.
    pub(super) fn start(&self, known: usize) -> Vec<u32> {
        let mut version = vec![0; self.members.len()];
        for run in 0..known {
            version[self.chain[run] as usize] = self.place[run] + 1;
        }
        version
    }

    /// Sets `current` to a run's history plus the run itself.
    pub(super) fn arrive(&self, run: usize, current: &mut [u32]) {
        let row = self.version_vectors.row(run);
        current[..row.len()].copy_from_slice(row);
        current[row.len()..].fill(0);
        current[self.chain[run] as usize] = self.place[run] + 1;
    }
}
