mod walk;

pub use walk::{walk, WalkStep};

use std::ops::Range;

/// An event's number on this replica, in the order it was learned of.
pub type EventIndex = u32;

/// Index of an event run among those the graph holds, in order of their first events. Shifts by
/// one when an earlier run splits.
pub type EventRunIndex = u32;

/// The causal graph of a document's history: which events each event's author had seen, without
/// their operations. Merging is only needed where history is concurrent, so the graph is stored
/// as event runs, with the critical versions that every path passes through kept up to date.
#[derive(Default)]
pub struct CausalGraph {
    runs: EventRuns,
    /// Events nothing descends from yet, ascending.
    frontier: Vec<EventIndex>,
    /// Per run, how many events from the start of history it has seen: exact if it has seen
    /// all before it, else a lower bound.
    seen_prefix_len: Vec<EventIndex>,
    /// Runs that start at a critical version, ascending.
    critical_versions: Vec<EventRunIndex>,
}

impl CausalGraph {
    /// Creates an empty graph.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a graph of only the events after the first `events`. Every later event must have
    /// seen `frontier`, the frontier of those first events.
    pub fn after(events: EventIndex, frontier: &[EventIndex]) -> Self {
        CausalGraph { runs: EventRuns { start: vec![events], ..EventRuns::default() }, frontier: frontier.to_vec(), ..Self::default() }
    }

    /// Returns the first event the graph holds.
    pub fn first_held_event(&self) -> EventIndex {
        self.runs.start[0]
    }

    /// Returns the number of events.
    pub fn len(&self) -> EventIndex {
        self.runs.start(self.runs.len())
    }

    /// Tells whether the graph has no events, held or before its first held event.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the events nothing descends from yet, ascending.
    pub fn frontier(&self) -> &[EventIndex] {
        &self.frontier
    }

    /// Tells whether an event with these parents just extends the last event.
    pub fn extends_last_event(&self, parents: &[EventIndex]) -> bool {
        !self.runs.is_empty() && parents == [self.len() - 1]
    }

    /// If some later event has not seen every event before `at`, starts a run at `at` and
    /// returns true.
    pub fn start_run_at(&mut self, at: EventIndex) -> bool {
        if at <= self.first_held_event() || at >= self.len() {
            return false;
        }

        let run = self.runs.containing(at);
        let critical = |run: EventRunIndex| self.critical_versions.binary_search(&run).is_ok();
        if self.runs.start(run) == at {
            return !critical(run);
        }

        // Partway along a run, every later event has what comes before unless something is
        // concurrent with the run.
        if critical(run) && (run + 1 == self.runs.len() || critical(run + 1)) {
            return false;
        }

        self.split_run(run, at);
        true
    }

    /// Returns the graph's event runs.
    pub fn runs(&self) -> &EventRuns {
        &self.runs
    }

    /// Appends a chain of `len` events, the first with `parents`. Returns their ids. `len` must
    /// not be 0, so that there are never more runs than events, and `parents` must be known
    /// events in ascending order.
    pub fn push(&mut self, parents: &[EventIndex], len: u32) -> Range<EventIndex> {
        let start = self.len();
        debug_assert!(len > 0, "a run of no events");
        debug_assert!(parents.is_sorted_by(|a, b| a < b), "parents not in ascending order");
        debug_assert!(parents.iter().all(|&parent| parent < start), "a parent is not a known event");
        let end = start.checked_add(len).expect("too many events");
        if self.extends_last_event(parents) {
            // Nothing else can have the last event as a parent yet, so its run just grows, and
            // the run's new end replaces the last event of the frontier.
            *self.runs.start.last_mut().expect("a run to extend") = end;
            *self.frontier.last_mut().expect("the last event is in the frontier") = end - 1;
            return start..end;
        }

        // The new run's parent runs go straight onto the end of the list of parents, and a parent
        // partway along a run splits it there. A split renumbers only the runs from the one it
        // splits on, and the parents come in ascending order and are concurrent, so no two are
        // in one run: the runs of the parents before stay as they are.
        let first_held = self.first_held_event();
        // Parents from before the graph begins are in every event's history, and name no run.
        for &parent in parents.iter().filter(|&&parent| parent >= first_held) {
            let run = self.runs.containing(parent);
            if parent + 1 < self.runs.start(run + 1) {
                self.split_run(run, parent + 1);
            }
            self.runs.parents.push(run);
        }
        let own_parents = *self.runs.parent_start.last().expect("a start for the parents of each run, and one after") as usize..;
        // An event whose parents are the whole frontier has seen everything, and becomes the
        // whole frontier. Compared one by one: comparing slices calls out to compare memory,
        // which costs more than the one or two events here.
        let merges_all = parents.len() == self.frontier.len() && parents.iter().zip(&self.frontier).all(|(parent, event)| parent == event);
        let seen =
            if merges_all { start } else { self.runs.parents[own_parents].iter().map(|&run| self.seen_prefix_len_after(run)).max().unwrap_or(first_held) };
        let new = self.runs.len();
        self.runs.parent_start.push(self.runs.parents.len() as u32);
        self.runs.start.push(end);
        self.seen_prefix_len.push(seen);

        // A version is critical only while every later run has seen all that came before it.
        while self.critical_versions.last().is_some_and(|&run| self.runs.start(run) > seen) {
            self.critical_versions.pop();
        }
        if seen == start {
            self.critical_versions.push(new);
        }
        if merges_all {
            self.frontier.clear();
        } else {
            self.frontier.retain(|event| !parents.contains(event));
        }
        self.frontier.push(end - 1);
        start..end
    }

    /// Returns how many events from the start of history a run's last event has seen.
    fn seen_prefix_len_after(&self, run: EventRunIndex) -> EventIndex {
        let i = run as usize;
        if self.seen_prefix_len[i] == self.runs.start[i] {
            self.runs.start[i + 1]
        } else {
            self.seen_prefix_len[i]
        }
    }

    /// Splits a run so that a new one starts at `at`.
    fn split_run(&mut self, run: EventRunIndex, at: EventIndex) {
        let runs = &mut self.runs;
        let (first, second, i) = (run, run + 1, run as usize);
        // Later runs named this one for its last event, which the second part now has; and
        // every run after it moves up one.
        let later = runs.parent_start[i + 1] as usize;
        insert_and_grow(&mut runs.parents, later, first, |parent| EventRunIndex::from(parent >= first));
        runs.start.insert(i + 1, at);
        insert_and_grow(&mut runs.parent_start, i + 1, later as u32, |_| 1);
        self.seen_prefix_len.insert(i + 1, if self.seen_prefix_len[i] == runs.start[i] { at } else { self.seen_prefix_len[i] });

        // Splits land near the end, after nearly every critical version, so those to renumber
        // are found from the end.
        let mut moved = self.critical_versions.len();
        while moved > 0 && self.critical_versions[moved - 1] > first {
            moved -= 1;
            self.critical_versions[moved] += 1;
        }
        if self.seen_prefix_len[i + 1..].iter().all(|&seen| seen >= at) {
            self.critical_versions.insert(moved, second);
        }
    }

    /// Splits the history at its critical versions.
    pub fn split_at_critical_versions(&self) -> Vec<HistoryRange> {
        let runs = self.runs.len();
        let critical = &self.critical_versions;
        let next = |i: usize| critical.get(i + 1).copied().unwrap_or(runs);
        let mut ranges = Vec::new();
        let mut i = 0;
        while i < critical.len() {
            let from = critical[i];
            if next(i) == from + 1 {
                // Critical on both sides: nothing is concurrent with this run. Take every run
                // that follows in the same way.
                while i + 1 < critical.len() && next(i + 1) == critical[i + 1] + 1 {
                    i += 1;
                }
                ranges.push(HistoryRange::Linear(from..next(i)));
            } else {
                ranges.push(HistoryRange::Concurrent(from..next(i)));
            }
            i += 1;
        }
        ranges
    }
}

/// Inserts `item` into `list` at `at`, adding `grow(old)` to each item after it as it moves along
/// one. Splits land near the end, so one pass from there costs less than moving the rest and then
/// passing over it again.
fn insert_and_grow(list: &mut Vec<u32>, at: usize, item: u32, grow: impl Fn(u32) -> u32) {
    list.push(item);
    for i in (at + 1..list.len()).rev() {
        list[i] = list[i - 1] + grow(list[i - 1]);
    }
    list[at] = item;
}

/// The graph cut into event runs: maximal chains of events that nothing branches off partway,
/// so a version holds a run whole or not at all.
pub struct EventRuns {
    /// Run `i` covers events `start[i]..start[i + 1]`.
    start: Vec<EventIndex>,
    parent_start: Vec<u32>,
    /// Each run's parent runs: those its first event's parents end.
    parents: Vec<EventRunIndex>,
}

impl Default for EventRuns {
    fn default() -> Self {
        EventRuns { start: vec![0], parent_start: vec![0], parents: Vec::new() }
    }
}

/// A stretch of history between two critical versions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HistoryRange {
    /// Runs nothing is concurrent with: their events apply as recorded.
    Linear(Range<EventRunIndex>),
    /// Runs with concurrency among them, which have to be merged.
    Concurrent(Range<EventRunIndex>),
}

impl HistoryRange {
    /// Returns the runs in the range.
    pub fn runs(&self) -> &Range<EventRunIndex> {
        match self {
            HistoryRange::Linear(runs) | HistoryRange::Concurrent(runs) => runs,
        }
    }
}

impl EventRuns {
    /// Returns the number of runs.
    pub fn len(&self) -> EventRunIndex {
        (self.parent_start.len() - 1) as EventRunIndex
    }

    /// Tells whether there are no runs.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns a run's first event. One past the last run gives the number of events.
    pub fn start(&self, run: EventRunIndex) -> EventIndex {
        self.start[run as usize]
    }

    /// Returns the events in a run.
    pub fn events(&self, run: EventRunIndex) -> Range<EventIndex> {
        self.start(run)..self.start(run + 1)
    }

    /// Returns the runs that a run's first event has as parents.
    pub fn parents(&self, run: EventRunIndex) -> &[EventRunIndex] {
        let i = run as usize;
        &self.parents[self.parent_start[i] as usize..self.parent_start[i + 1] as usize]
    }

    /// Finds the run holding an event.
    pub fn containing(&self, event: EventIndex) -> EventRunIndex {
        // Parents are mostly recent events: the last few runs are tried one by one, and from
        // there the search widens backwards.
        let len = self.len() as usize;
        let near = len.saturating_sub(4);
        if let Some(run) = (near..len).rev().find(|&run| self.start[run] <= event) {
            return run as EventRunIndex;
        }
        let (mut end, mut step) = (near, 1);
        while step < end && self.start[end - step] > event {
            end -= step;
            step *= 2;
        }
        let from = end.saturating_sub(step);
        (from + self.start[from..end].partition_point(|&start| start <= event) - 1) as EventRunIndex
    }
}
