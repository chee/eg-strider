use std::ops::Range;
use strider_graph::EventRunIndex;

/// Where merging put each operation of the concurrent ranges: the paper's transformed
/// operations, as positions in the effect version. The rest of each operation is the operation
/// run itself.
#[derive(Default)]
pub(crate) struct TransformedOperations {
    /// Ranges of event runs, in merge order. An operation run that goes from one event run into
    /// the next is one operation run.
    pub(crate) merge_order: Vec<Range<EventRunIndex>>,
    /// Per operation run of those ranges. An insert: the index it went in at. A delete: the index
    /// and length of each piece it removed, then the number of pieces.
    pub(crate) positions: Vec<u32>,
    /// Pieces the current delete has removed so far.
    pub(crate) removed_pieces: u32,
    /// Length of the document so far.
    pub(crate) len: u32,
    /// The first run whose events the document does not hold. Operations of earlier runs are not
    /// recorded.
    pub(crate) first_new_run: EventRunIndex,
    /// Whether the current range of runs is of those earlier runs.
    pub(crate) in_base: bool,
}

impl TransformedOperations {
    /// Starts on a range of runs, which the document holds all of or none of.
    pub(crate) fn begin(&mut self, run_range: Range<EventRunIndex>) {
        self.in_base = run_range.end <= self.first_new_run;
        debug_assert!(self.in_base || run_range.start >= self.first_new_run, "runs with events on both sides of the base");
        if !self.in_base {
            self.merge_order.push(run_range);
        }
    }

    /// Records that an insert of `len` characters went in at `pos`.
    pub(crate) fn insert(&mut self, pos: u32, len: u32) {
        if !self.in_base {
            self.len += len;
            self.positions.push(pos);
        }
    }

    /// Records that the current delete removed the `len` characters at `pos`.
    pub(crate) fn remove(&mut self, pos: u32, len: u32) {
        if !self.in_base {
            self.len -= len;
            self.positions.extend([pos, len]);
            self.removed_pieces += 1;
        }
    }

    /// Ends a delete once all it removed is recorded.
    pub(crate) fn end_delete(&mut self) {
        if !self.in_base {
            self.positions.push(std::mem::take(&mut self.removed_pieces));
        }
    }

    /// Marks where things stand, for [`undo_to`](Self::undo_to).
    pub(crate) fn mark(&self) -> (usize, usize, u32) {
        (self.merge_order.len(), self.positions.len(), self.len)
    }

    /// Forgets what was recorded since `mark`.
    pub(crate) fn undo_to(&mut self, mark: (usize, usize, u32)) {
        self.merge_order.truncate(mark.0);
        self.positions.truncate(mark.1);
        self.len = mark.2;
    }
}
