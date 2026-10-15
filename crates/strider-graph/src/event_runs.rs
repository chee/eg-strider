use crate::{EventIndex, EventRunIndex};
use std::ops::Range;

/// The graph cut into event runs: maximal chains of events that nothing branches off partway,
/// so a version holds a run whole or not at all.
pub struct EventRuns {
    /// Run `i` covers events `start[i]..start[i + 1]`.
    pub(crate) start: Vec<EventIndex>,
    pub(crate) parent_start: Vec<u32>,
    /// Each run's parent runs: those its first event's parents end.
    pub(crate) parents: Vec<EventRunIndex>,
}

impl Default for EventRuns {
    fn default() -> Self {
        EventRuns { start: vec![0], parent_start: vec![0], parents: Vec::new() }
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
