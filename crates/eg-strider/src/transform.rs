use crate::oplog::{OpLog, OperationKind, OperationRunIndex};
use crate::replay::TransformedOperations;
use std::ops::Range;
use strider_graph::{EventRunIndex, EventRuns};

/// Concurrent ranges with more runs go straight to the CRDT records. The sets kept per run grow
/// with its square: 256 KB here.
const MAX_RUNS: usize = 1024;

/// Beyond this many edited ranges, scanning them for every operation costs more than the CRDT
/// records would.
const MAX_EDITED_RANGES: usize = 64;

/// A run's place among its concurrent range's runs, counting from 0.
type RunInRange = u32;

/// A set of a concurrent range's runs, one bit each, 64 to a word.
type RunSet = [u64];

/// A part of the document that the range's runs have edited, measured in the document as merged
/// so far.
struct EditedRange {
    start: i64,
    len: i64,
    /// The runs that edited here and are not stable, and how much each changed its length.
    members: Vec<(RunInRange, i64)>,
}

impl EditedRange {
    /// Records that `run` changed the edited range's length by `grew`.
    fn add(&mut self, run: RunInRange, grew: i64) {
        match self.members.last_mut() {
            Some((last, by)) if *last == run => *by += grew,
            _ => self.members.push((run, grew)),
        }
    }
}

/// The run whose operations are being transformed.
struct TransformingRun<'a> {
    run: RunInRange,
    /// The range's runs in its version, itself included.
    version: &'a RunSet,
    /// The runs that it and every later run have seen: those causally stable.
    stable: &'a RunSet,
}

/// Tells whether `run` is in `set`.
fn has(set: &RunSet, run: RunInRange) -> bool {
    set[run as usize / 64] >> (run % 64) & 1 == 1
}

/// The parts of the document that a concurrent range's runs have edited, in document order.
#[derive(Default)]
struct EditedRanges {
    list: Vec<EditedRange>,
    spare: Vec<Vec<(RunInRange, i64)>>,
}

impl EditedRanges {
    /// Empties the list, keeping its allocations for the next concurrent range.
    fn clear(&mut self) {
        for mut edited in self.list.drain(..) {
            edited.members.clear();
            self.spare.push(edited.members);
        }
    }

    /// Returns how much longer edited range `i` is than `transforming` knows, and whether any run
    /// it has not seen edited there. Stable runs are forgotten first; an edited range left empty
    /// is dropped and `None` returned, with `i` then naming the next one.
    fn unseen(&mut self, i: usize, transforming: &TransformingRun) -> Option<(i64, bool)> {
        let (mut longer, mut any, mut stale) = (0, false, false);
        for &(run, grew) in &self.list[i].members {
            if has(transforming.stable, run) {
                stale = true;
            } else if !has(transforming.version, run) {
                longer += grew;
                any = true;
            }
        }
        // Rewriting the list only when something has to go keeps the common case to a read.
        if stale {
            self.list[i].members.retain(|&(run, _)| !has(transforming.stable, run));
        }
        if self.list[i].members.is_empty() {
            self.spare.push(self.list.remove(i).members);
            return None;
        }
        Some((longer, any))
    }

    /// Adds an edited range at `i`, unless there are too many.
    fn open(&mut self, i: usize, start: i64, len: i64, transforming: &TransformingRun, grew: i64) -> Option<()> {
        if self.list.len() == MAX_EDITED_RANGES {
            return None;
        }
        let mut members = self.spare.pop().unwrap_or_default();
        members.push((transforming.run, grew));
        self.list.insert(i, EditedRange { start, len, members });
        Some(())
    }

    /// Places an insert of `len` characters at `pos`, in the document as `transforming` knows it,
    /// in the document as merged so far.
    fn insert(&mut self, transforming: &TransformingRun, pos: i64, len: i64) -> Option<i64> {
        let own = !has(transforming.stable, transforming.run);
        // `shift`: how much further along the merged document is than the transforming run's.
        let (mut i, mut shift, mut inside) = (0, 0, false);
        while i < self.list.len() {
            let Some((longer, any)) = self.unseen(i, transforming) else { continue };
            // The edited range as the transforming run knows it.
            let start = self.list[i].start - shift;
            let end = start + self.list[i].len - longer;
            if pos < start {
                break;
            }
            if pos <= end {
                if any {
                    return None;
                }
                inside = true;
                break;
            }
            shift += longer;
            i += 1;
        }
        let at = pos + shift;
        if inside {
            self.list[i].len += len;
            if own {
                self.list[i].add(transforming.run, len);
            }
            i += 1;
        } else if own {
            self.open(i, at, len, transforming, len)?;
            i += 1;
        }
        for later in &mut self.list[i..] {
            later.start += len;
        }
        Some(at)
    }

    /// Places a removal of the `len` characters at `pos`, in the document as `transforming` knows
    /// it, in the document as merged so far.
    fn remove(&mut self, transforming: &TransformingRun, pos: i64, len: i64) -> Option<i64> {
        let own = !has(transforming.stable, transforming.run);
        // Edited ranges that end before the removed text, with a character in between.
        let (mut i, mut shift) = (0, 0);
        while i < self.list.len() {
            let Some((longer, _)) = self.unseen(i, transforming) else { continue };
            if self.list[i].start - shift + self.list[i].len - longer >= pos {
                break;
            }
            shift += longer;
            i += 1;
        }
        // Edited ranges `i..j` that the removed text reaches into or touches.
        let (mut j, mut shift_at_start, mut shift_at_end) = (i, shift, shift);
        while j < self.list.len() {
            let Some((longer, any)) = self.unseen(j, transforming) else { continue };
            let start = self.list[j].start - shift_at_end;
            let end = start + self.list[j].len - longer;
            if start > pos + len {
                break;
            }
            if end == pos {
                // Touches the removed text from before: all of it lies before.
                shift_at_start = shift_at_end + longer;
            } else if start < pos + len && any {
                return None;
            }
            shift_at_end += longer;
            j += 1;
        }
        let at = pos + shift_at_start;
        if i < j {
            // The removed text and everything it touches are one edited range from now on.
            let start = at.min(self.list[i].start);
            let end = (pos + len + shift_at_end).max(self.list[j - 1].start + self.list[j - 1].len);
            for _ in i + 1..j {
                let mut absorbed = self.list.remove(i + 1).members;
                self.list[i].members.extend_from_slice(&absorbed);
                absorbed.clear();
                self.spare.push(absorbed);
            }
            self.list[i].start = start;
            self.list[i].len = end - start - len;
            if own {
                self.list[i].add(transforming.run, -len);
            }
            i += 1;
        } else if own {
            self.open(i, at, 0, transforming, -len)?;
            i += 1;
        }
        for later in &mut self.list[i..] {
            later.start -= len;
        }
        Some(at)
    }
}

/// Merges a concurrent range without the CRDT records when its concurrent operations do not
/// touch: the simple case of operational transformation, moving each operation by the net length
/// of the concurrent operations before it. The places edited are kept as edited ranges, and an
/// operation landing in an edited range that holds operations it has not seen sends the range to
/// the CRDT records. A run is stable once every later run has seen it.
#[derive(Default)]
pub(crate) struct Transform {
    /// Per run of the range: the range's runs in its version, itself included.
    versions: Vec<u64>,
    /// Per run of the range: the runs that it and every later run have seen.
    stable: Vec<u64>,
    edited: EditedRanges,
}

impl Transform {
    /// Merges a concurrent range by transforming, recording in `transformed` where its operations
    /// go. Returns false, leaving `transformed` as it was, if the range needs the CRDT records.
    pub fn try_merge(
        &mut self,
        log: &OpLog,
        runs: &EventRuns,
        first_op_of_run: &[OperationRunIndex],
        range: &Range<EventRunIndex>,
        transformed: &mut TransformedOperations,
    ) -> bool {
        if range.len() > MAX_RUNS {
            return false;
        }
        let words = self.fill_versions(runs, range);
        let Transform { versions, stable, edited } = self;
        edited.clear();

        let mark = transformed.mark();
        for (i, run) in (range.start..range.end).enumerate() {
            let transforming = TransformingRun { run: i as RunInRange, version: &versions[i * words..][..words], stable: &stable[i * words..][..words] };
            transformed.begin(run..run + 1);
            for op in log.ops(first_op_of_run[run as usize]..first_op_of_run[run as usize + 1], runs.events(run)) {
                let at = match op.kind {
                    OperationKind::Insert => edited.insert(&transforming, op.pos as i64, op.len as i64),
                    OperationKind::Delete => edited.remove(&transforming, op.pos as i64, op.len as i64),
                };
                match (at, op.kind) {
                    (Some(at), OperationKind::Insert) => transformed.insert(at as u32, op.len),
                    (Some(at), OperationKind::Delete) => {
                        transformed.remove(at as u32, op.len);
                        transformed.end_delete();
                    }
                    (None, _) => {
                        transformed.undo_to(mark);
                        return false;
                    }
                }
            }
        }
        true
    }

    /// Fills in `versions` and `stable` for the range. Returns how many words each set has.
    fn fill_versions(&mut self, runs: &EventRuns, range: &Range<EventRunIndex>) -> usize {
        let count = range.len();
        let words = count.div_ceil(64);
        self.versions.clear();
        self.stable.clear();
        if words == 1 {
            // Most concurrent ranges are this small: each set is one word, with no slices to cut.
            for i in 0..count {
                let inside = runs.parents(range.start + i as EventRunIndex).iter().filter_map(|&p| p.checked_sub(range.start)).map(|p| p as usize);
                let version = inside.fold(1 << i, |version, parent| version | self.versions[parent]);
                self.versions.push(version);
            }
            self.stable.resize(count, 0);
            let mut later = u64::MAX;
            for i in (0..count).rev() {
                later &= self.versions[i];
                self.stable[i] = later;
            }
            return words;
        }
        self.versions.resize(count * words, 0);
        for i in 0..count {
            let (earlier, own) = self.versions.split_at_mut(i * words);
            let own = &mut own[..words];
            // Parents from before the range are before a critical version: all have them.
            for parent in runs.parents(range.start + i as EventRunIndex).iter().filter_map(|&p| p.checked_sub(range.start)).map(|p| p as usize) {
                for (word, theirs) in own.iter_mut().zip(&earlier[parent * words..][..words]) {
                    *word |= theirs;
                }
            }
            own[i / 64] |= 1 << (i % 64);
        }
        self.stable.resize(count * words, u64::MAX);
        for i in (0..count).rev() {
            for word in 0..words {
                let later = if i + 1 < count { self.stable[(i + 1) * words + word] } else { u64::MAX };
                self.stable[i * words + word] = later & self.versions[i * words + word];
            }
        }
        words
    }
}
