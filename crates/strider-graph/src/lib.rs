//! The causal graph of a collaborative document's history.

mod walk;

pub use walk::{Move, Walker};

use std::ops::Range;

/// An append-only causal graph, run-length encoded.
#[derive(Default)]
pub struct Graph {
    entries: Vec<Entry>,
    parents: Vec<u32>,
    heads: Vec<u32>,
    len: u32,
}

/// Events `start..end`, where each after the first has the previous one as its only parent.
struct Entry {
    start: u32,
    end: u32,
    /// Range in `Graph::parents` holding the first event's parents.
    parents: (u32, u32),
}

impl Graph {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of events.
    pub fn len(&self) -> u32 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The events that nothing yet descends from, in ascending order.
    pub fn frontier(&self) -> &[u32] {
        // Kept in order: each new event is the highest so far, and goes on the end.
        &self.heads
    }

    /// Whether a new event with these parents would simply follow the last event.
    pub fn continues(&self, parents: &[u32]) -> bool {
        self.len > 0 && parents == [self.len - 1]
    }

    /// Adds `len` events in a chain, the first with the given parents. Returns their
    /// numbers.
    pub fn push(&mut self, parents: &[u32], len: u32) -> Range<u32> {
        let start = self.len;
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
            self.entries.last_mut().unwrap().end = end;
        } else {
            let at = self.parents.len() as u32;
            self.parents.extend_from_slice(parents);
            self.entries.push(Entry { start, end, parents: (at, self.parents.len() as u32) });
        }
        self.heads.retain(|h| !parents.contains(h));
        self.heads.push(end - 1);
        self.len = end;
        start..end
    }

    /// Cuts the graph into [`Runs`].
    pub fn runs(&self) -> Runs {
        let n = self.len;

        // A run starts wherever an entry does, and just after every event that is some
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

        // The run containing an event: one less than the number of starts at or before it.
        let run_of = |event: u32| {
            let below = cut[event as usize / 64] & (u64::MAX >> (63 - event % 64));
            rank[event as usize / 64] + below.count_ones() - 1
        };

        let mut parent_start = Vec::with_capacity(count as usize + 1);
        let mut parents = Vec::with_capacity(self.parents.len() + count as usize);
        let mut run = 0usize;
        for e in &self.entries {
            debug_assert_eq!(start[run], e.start);
            parent_start.push(parents.len() as u32);
            parents.extend(self.parents[e.parents.0 as usize..e.parents.1 as usize].iter().map(|&p| run_of(p)));
            run += 1;
            // Later pieces of the same entry each continue from the piece before.
            while start[run] < e.end {
                parent_start.push(parents.len() as u32);
                parents.push(run as u32 - 1);
                run += 1;
            }
        }
        parent_start.push(parents.len() as u32);

        Runs { start, parent_start, parents }
    }
}

/// A graph cut into *runs*: maximal chains of events that no other event branches from
/// part-way along. A version therefore contains a run entirely or not at all, which lets
/// versions be handled as sets of runs.
pub struct Runs {
    /// Run `i` covers events `start[i]..start[i + 1]`.
    start: Vec<u32>,
    parent_start: Vec<u32>,
    parents: Vec<u32>,
}

/// A stretch of history between two critical versions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Segment {
    /// Runs that nothing is concurrent with. Their events mean exactly what they say.
    Linear(Range<usize>),
    /// Runs with concurrency among them, which have to be merged.
    Zone(Range<usize>),
}

impl Segment {
    /// The runs in the segment.
    pub fn runs(&self) -> &Range<usize> {
        match self {
            Segment::Linear(runs) | Segment::Zone(runs) => runs,
        }
    }
}

impl Runs {
    /// Number of runs.
    pub fn len(&self) -> usize {
        self.parent_start.len() - 1
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The first event of a run. `run` may be one past the last run, giving the total
    /// number of events.
    pub fn start(&self, run: usize) -> u32 {
        self.start[run]
    }

    /// The events in a run.
    pub fn events(&self, run: usize) -> Range<u32> {
        self.start[run]..self.start[run + 1]
    }

    /// The runs a run's first event has as parents.
    pub fn parents(&self, run: usize) -> &[u32] {
        &self.parents[self.parent_start[run] as usize..self.parent_start[run + 1] as usize]
    }

    /// Splits the history at its critical versions: the points where every later event has
    /// every earlier event in its history, so that nothing before the point can affect how
    /// anything after it is merged.
    pub fn segments(&self) -> Vec<Segment> {
        let runs = self.len();
        let start = &self.start;

        // seen[i]: a `q` such that run i's first event has all of events 0..q in its
        // history. Exact when the run has seen everything before it (its parents are then
        // exactly the heads of the graph so far), otherwise a lower bound from its parents.
        let mut seen = vec![0u32; runs];
        let mut is_head = vec![false; runs];
        let mut heads = 0usize;
        for i in 0..runs {
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

        // The version just before run i is critical when every run from i on has seen all
        // of events 0..start[i].
        let mut least_seen = vec![u32::MAX; runs + 1];
        for i in (0..runs).rev() {
            least_seen[i] = least_seen[i + 1].min(seen[i]);
        }
        let critical = |i: usize| least_seen[i] >= start[i];

        let mut segments = Vec::new();
        let mut i = 0;
        while i < runs {
            debug_assert!(critical(i));
            let mut end = i + 1;
            if critical(end) {
                // Critical on both sides: nothing is concurrent with this run. Take every
                // run that follows in the same way.
                while end < runs && critical(end + 1) {
                    end += 1;
                }
                segments.push(Segment::Linear(i..end));
            } else {
                while !critical(end) {
                    end += 1;
                }
                segments.push(Segment::Zone(i..end));
            }
            i = end;
        }
        segments
    }
}
