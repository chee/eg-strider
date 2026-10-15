use crate::operation::OperationKind;
use std::cmp::Ordering;
use std::ops::Range;
use strider_graph::EventIndex;
use strider_tree::{Cursor, EntryId, Lengths, PrepareEffectTree, View};

/// A character the CRDT records know: the event of the concurrent range that inserted it, or one
/// of the document as the range began.
pub(crate) type RecordId = u32;

/// No record: the start of the document as a left origin, the end as a right origin.
pub(crate) const NO_RECORD: RecordId = u32::MAX;

/// Marks a character of the document as the concurrent range began, which the placeholder record
/// stands for: `PLACEHOLDER | position`.
const PLACEHOLDER: RecordId = 1 << 31;
/// The length of the placeholder record. Its real length is not needed.
const PLACEHOLDER_LEN: u32 = 0x7fff_0000;

// A record run's state in the prepare version. Each delete of an inserted character adds one.
const NOT_INSERTED_YET: u32 = 0;
const INSERTED: u32 = 1;

/// Characters of the document as the range began per entry of the index into the placeholder.
/// Tiny in debug builds, so that tests cross entries.
const PLACEHOLDER_INDEX_STRIDE: u32 = if cfg!(debug_assertions) { 4 } else { 64 };

/// Tree node size. Tiny in debug builds, so that tests split nodes.
const NODE: usize = if cfg!(debug_assertions) { 4 } else { 32 };

#[derive(Clone, Copy)]
struct RecordRun {
    /// The first character; the rest follow consecutively.
    record: RecordId,
    len: u32,
    /// The record left of the first character when it was inserted. Each later character's left
    /// origin is the one before it.
    origin_left: RecordId,
    /// The first record to the right that existed when these were inserted.
    origin_right: RecordId,
    prepare_state: u32,
    deleted_in_effect: bool,
}

/// A stretch of delete events whose targets are consecutive records: the first event's is the
/// lowest, or the highest if `backward`.
#[derive(Clone, Copy)]
pub(crate) struct DeleteTargets {
    /// The first of the events.
    pub first_event: EventIndex,
    pub len: u32,
    /// The lowest record deleted.
    pub record: RecordId,
    pub backward: bool,
}

impl DeleteTargets {
    /// Returns the record that the first event deletes.
    pub fn first(&self) -> RecordId {
        if self.backward {
            self.record + self.len - 1
        } else {
            self.record
        }
    }

    /// Returns the part of the stretch from `event`, at most `len` events long.
    fn from(&self, event: EventIndex, len: u32) -> DeleteTargets {
        let (skip, len) = (event - self.first_event, len.min(self.first_event + self.len - event));
        let record = if self.backward { self.record + self.len - skip - len } else { self.record + skip };
        DeleteTargets { first_event: event, len, record, backward: self.backward }
    }
}

impl RecordRun {
    /// Returns the run's lengths in the prepare and effect versions.
    fn lengths(&self) -> Lengths {
        Lengths { prepare: if self.prepare_state == INSERTED { self.len } else { 0 }, effect: if self.deleted_in_effect { 0 } else { self.len } }
    }
}

/// A position between two characters.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Position {
    /// This many characters into the run at this entry, fewer than its length.
    In(Cursor, u32),
    End,
}

/// The records of the CRDT for merging one concurrent range, as in Eg-walker: the range's
/// characters as record runs with left and right origins, as in Yjs or Fugue, in a B+ tree
/// measured in the prepare and effect versions. Merging an event finds its position in the
/// prepare version and reads it off in the effect version.
pub(crate) struct CrdtRecords {
    /// The record runs in document order, measured in both versions.
    tree: PrepareEffectTree<NODE>,
    /// By tree entry id.
    record_runs: Vec<RecordRun>,
    /// Per event of the range: for an insert, the run holding its character; for a delete, its
    /// index in `delete_targets`.
    record_of_event: Vec<u32>,
    /// What the delete events deleted, a stretch of consecutive records at a time.
    delete_targets: Vec<DeleteTargets>,
    /// Per entry of that index, the placeholder run holding its first character.
    placeholder_index: Vec<EntryId>,
    /// Per run, the `integrate_scan_stamp` of the last `integrate` scan over it.
    integrate_scan_marks: Vec<u32>,
    integrate_scan_stamp: u32,
}

impl CrdtRecords {
    /// Starts the records of a concurrent range of `events` events with one placeholder record,
    /// for the document as the range began, `len` characters long.
    pub fn new(events: usize, len: u32) -> Self {
        let placeholder = RecordRun {
            record: PLACEHOLDER,
            len: PLACEHOLDER_LEN,
            origin_left: NO_RECORD,
            origin_right: NO_RECORD,
            prepare_state: INSERTED,
            deleted_in_effect: false,
        };
        CrdtRecords {
            tree: PrepareEffectTree::new(placeholder.lengths()),
            record_runs: vec![placeholder],
            record_of_event: vec![NO_RECORD; events],
            delete_targets: Vec::new(),
            placeholder_index: vec![0; (len / PLACEHOLDER_INDEX_STRIDE) as usize + 1],
            integrate_scan_marks: Vec::new(),
            integrate_scan_stamp: 0,
        }
    }

    // ---------------------------------------------------------------- finding things

    /// Returns the run stored at a tree entry.
    fn record_run(&self, at: Cursor) -> &RecordRun {
        &self.record_runs[self.tree.id(at) as usize]
    }

    /// Finds the run holding `record`, and the offset into it.
    fn locate(&self, record: RecordId) -> (EntryId, u32) {
        let run = if record & PLACEHOLDER != 0 { self.placeholder(record) } else { self.record_of_event[record as usize] };
        (run, record - self.record_runs[run as usize].record)
    }

    /// Finds the run holding a placeholder record, starting from the run that holds its index
    /// entry's first character.
    fn placeholder(&self, record: RecordId) -> EntryId {
        let holds = |id: u32| {
            let run = &self.record_runs[id as usize];
            run.record & PLACEHOLDER != 0 && record.wrapping_sub(run.record) < run.len
        };
        let mut id = self.placeholder_index[((record & !PLACEHOLDER) / PLACEHOLDER_INDEX_STRIDE) as usize];
        if holds(id) {
            return id;
        }
        let mut at = self.tree.cursor(id);
        loop {
            at = self.tree.next(at).expect("a placeholder record is in some run");
            id = self.tree.id(at);
            if holds(id) {
                return id;
            }
        }
    }

    /// Returns the position `offset` characters into the run at `at`. The run's full length is
    /// the start of the next run.
    fn position_in(&self, at: Cursor, offset: u32) -> Position {
        if offset < self.record_run(at).len {
            Position::In(at, offset)
        } else {
            self.tree.next(at).map_or(Position::End, |next| Position::In(next, 0))
        }
    }

    /// Finds the entry holding `record` and the position just after it; for `NO_RECORD`, nothing
    /// and the start of the document.
    fn after(&self, record: RecordId) -> (Option<Cursor>, Position) {
        if record == NO_RECORD {
            return (None, Position::In(self.tree.first(), 0));
        }
        let (run, offset) = self.locate(record);
        let at = self.tree.cursor(run);
        (Some(at), self.position_in(at, offset + 1))
    }

    /// Returns the position just before `record`, or the end of the document for `NO_RECORD`.
    fn position_before(&self, record: RecordId) -> Position {
        if record == NO_RECORD {
            return Position::End;
        }
        let (run, offset) = self.locate(record);
        Position::In(self.tree.cursor(run), offset)
    }

    /// Compares where two positions are in the document.
    fn cmp(&self, a: Position, b: Position) -> Ordering {
        match (a, b) {
            (Position::End, Position::End) => Ordering::Equal,
            (Position::End, _) => Ordering::Greater,
            (_, Position::End) => Ordering::Less,
            (Position::In(x, i), Position::In(y, j)) => self.tree.cmp(x, y).then(i.cmp(&j)),
        }
    }

    // ---------------------------------------------------------------- splitting runs

    /// Splits the run at `at` after `offset` characters. Returns both parts; one keeps the
    /// run's id.
    fn split(&mut self, at: Cursor, offset: u32) -> [Cursor; 2] {
        let run = self.tree.id(at);
        let whole = self.record_runs[run as usize];
        debug_assert!(offset > 0 && offset < whole.len);
        let left = RecordRun { len: offset, ..whole };
        let right = RecordRun { record: whole.record + offset, len: whole.len - offset, origin_left: whole.record + offset - 1, ..whole };
        let new = self.record_runs.len() as u32;
        let events = |s: &RecordRun| s.record as usize..(s.record + s.len) as usize;

        // Every event in the part that takes the new id has to be re-pointed, so give the
        // new id to the smaller part: each event is then re-pointed O(log) times. Likewise
        // every index entry that begins in a placeholder part.
        let placeholder = whole.record & PLACEHOLDER != 0;
        let index = if placeholder { &mut self.placeholder_index } else { &mut self.record_of_event };
        let pointed = |s: &RecordRun| {
            if placeholder {
                let page = |pos: u32| ((pos & !PLACEHOLDER).div_ceil(PLACEHOLDER_INDEX_STRIDE) as usize).min(index.len());
                page(s.record)..page(s.record + s.len)
            } else {
                events(s)
            }
        };
        let (from_left, from_right) = (pointed(&left), pointed(&right));
        if from_right.len() <= from_left.len() {
            self.record_runs[run as usize] = left;
            self.record_runs.push(right);
            index[from_right].fill(new);
            self.tree.split_entry(at, right.lengths(), false).1
        } else {
            self.record_runs[run as usize] = right;
            self.record_runs.push(left);
            index[from_left].fill(new);
            self.tree.split_entry(at, left.lengths(), true).1
        }
    }

    /// Splits the run at `at` so that one run covers exactly `len` characters from `offset`,
    /// and returns it.
    fn isolate(&mut self, mut at: Cursor, offset: u32, len: u32) -> Cursor {
        if offset > 0 {
            at = self.split(at, offset)[1];
        }
        if len < self.record_run(at).len {
            at = self.split(at, len)[0];
        }
        at
    }

    // ---------------------------------------------------------------- inserting

    /// Merges an insert of `len` characters at prepare index `pos`, made by events of the range
    /// `event..event + len`. Returns its effect index.
    pub fn insert(&mut self, event: EventIndex, len: u32, pos: u32, goes_before: impl Fn(EventIndex, EventIndex) -> bool) -> u32 {
        // `index`: where the position just after the left origin is in the effect version, which
        // finding the origin gives. The origin's own character is there unless it was deleted.
        let (origin_left, left, after_left, index) = if pos == 0 {
            (NO_RECORD, None, Position::In(self.tree.first(), 0), 0)
        } else {
            let (at, offset, start) = self.tree.locate(View::Prepare, pos - 1);
            let run = self.record_run(at);
            (run.record + offset, Some(at), self.position_in(at, offset + 1), start + if run.deleted_in_effect { 0 } else { offset + 1 })
        };

        // The first thing to our right that already existed, deleted or not. Anything
        // before it is a concurrent insert that we have to be ordered against.
        let mut concurrent = false;
        let origin_right = match after_left {
            Position::End => NO_RECORD,
            Position::In(at, offset) if offset > 0 => self.record_run(at).record + offset,
            Position::In(mut at, _) => loop {
                let run = self.record_run(at);
                if run.prepare_state != NOT_INSERTED_YET {
                    break run.record;
                }
                concurrent = true;
                match self.tree.next(at) {
                    Some(next) => at = next,
                    None => break NO_RECORD,
                }
            },
        };

        // Among concurrent inserts the position may be further along, and is found by climbing.
        let position = if concurrent { self.integrate(after_left, event, origin_left, origin_right, &goes_before) } else { after_left };
        self.insert_record_run(position, left.filter(|_| position == after_left), event, len, origin_left, origin_right, (!concurrent).then_some(index))
    }

    /// Merges an insert whose origins are known from [`origins_of`](Self::origins_of), needing no
    /// prepare version.
    pub fn insert_with_known_origins(
        &mut self,
        event: EventIndex,
        len: u32,
        origin_left: RecordId,
        origin_right: RecordId,
        goes_before: impl Fn(EventIndex, EventIndex) -> bool,
    ) -> u32 {
        let (left, after_left) = self.after(origin_left);
        let next = match after_left {
            Position::End => NO_RECORD,
            Position::In(at, offset) => self.record_run(at).record + offset,
        };
        // Everything between the two origins was inserted concurrently with this event.
        let position = if next != origin_right { self.integrate(after_left, event, origin_left, origin_right, &goes_before) } else { after_left };
        self.insert_record_run(position, left.filter(|_| position == after_left), event, len, origin_left, origin_right, None)
    }

    /// Finds where a new insert goes among the concurrent inserts starting at `start`, just
    /// after its left origin.
    fn integrate(
        &mut self,
        start: Position,
        event: EventIndex,
        origin_left: RecordId,
        origin_right: RecordId,
        goes_before: &impl Fn(EventIndex, EventIndex) -> bool,
    ) -> Position {
        // Runs passed over are stamped, so that "is this record in the stretch already
        // scanned?" is one array read rather than a comparison of positions.
        self.integrate_scan_stamp += 1;
        let stamp = self.integrate_scan_stamp;
        self.integrate_scan_marks.resize(self.record_runs.len(), 0);
        let Position::In(first, first_off) = start else { return start };
        let first = self.tree.id(first);

        let mut here = start;
        // Where to land if the scan ends without settling it: before the first of a series
        // of inserts that share our left origin but reach less far to the right.
        let mut fallback = None;
        let landed = loop {
            let Position::In(at, offset) = here else { break here };
            let id = self.tree.id(at);
            let run = self.record_runs[id as usize];
            let other = run.record + offset;
            if other == origin_right {
                break here;
            }
            self.integrate_scan_marks[id as usize] = stamp;
            let other_left = if offset == 0 { run.origin_left } else { other - 1 };

            // Where the other insert's left origin lies relative to ours. If it is not the
            // same record, it is either inside the stretch scanned so far (the other insert
            // descends from something we have passed) or to the left of it.
            let order = if other_left == origin_left {
                Ordering::Equal
            } else if other_left == NO_RECORD {
                Ordering::Less
            } else {
                let (left_run, left_off) = self.locate(other_left);
                if self.integrate_scan_marks[left_run as usize] == stamp && (left_run != first || left_off >= first_off) {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            };
            debug_assert_eq!(order, self.cmp(self.after(other_left).1, start));

            match order {
                Ordering::Less => break here,
                Ordering::Greater => {}
                Ordering::Equal if run.origin_right == origin_right => {
                    if goes_before(event, other) {
                        break here;
                    }
                    fallback = None;
                }
                Ordering::Equal => {
                    let ends_sooner = self.cmp(self.position_before(run.origin_right), self.position_before(origin_right)) == Ordering::Less;
                    fallback = if ends_sooner { fallback.or(Some(here)) } else { None };
                }
            }
            here = self.tree.next(at).map_or(Position::End, |next| Position::In(next, 0));
        };
        fallback.unwrap_or(landed)
    }

    /// Adds the characters of events `event..event + len` at `position`, returning their effect
    /// index: `index` if given, else found by climbing the tree. `left` is the entry holding
    /// the left origin, given only while `position` is right after it.
    #[allow(clippy::too_many_arguments)]
    fn insert_record_run(
        &mut self,
        position: Position,
        left: Option<Cursor>,
        event: EventIndex,
        len: u32,
        origin_left: RecordId,
        origin_right: RecordId,
        index: Option<u32>,
    ) -> u32 {
        // Typing on from the character before: extend its run instead of adding one.
        if let Some(at) = left {
            let id = self.tree.id(at);
            let prev = &mut self.record_runs[id as usize];
            let follows = prev.record.wrapping_add(prev.len) == event && origin_left == event.wrapping_sub(1);
            if follows && prev.origin_right == origin_right && prev.prepare_state == INSERTED && !prev.deleted_in_effect {
                prev.len += len;
                let (lengths, old_len) = (prev.lengths(), prev.len - len);
                self.record_of_event[event as usize..(event + len) as usize].fill(id);
                return match index {
                    Some(index) => {
                        self.tree.set_lengths(at, lengths);
                        index
                    }
                    None => self.tree.update(at, lengths) + old_len,
                };
            }
        }

        let before = match position {
            Position::End => None,
            Position::In(at, 0) => Some(at),
            Position::In(at, offset) => Some(self.split(at, offset)[1]),
        };
        let run = RecordRun { record: event, len, origin_left, origin_right, prepare_state: INSERTED, deleted_in_effect: false };
        let (id, index) = match index {
            Some(index) => (self.tree.add(before, run.lengths()), index),
            None => self.tree.insert(before, run.lengths()),
        };
        debug_assert_eq!(id as usize, self.record_runs.len());
        self.record_runs.push(run);
        self.record_of_event[event as usize..(event + len) as usize].fill(id);
        index
    }

    /// Returns the left and right origins of the character inserted by event `event` of the range.
    pub fn origins_of(&self, event: EventIndex) -> [RecordId; 2] {
        let run = &self.record_runs[self.record_of_event[event as usize] as usize];
        [if event == run.record { run.origin_left } else { event - 1 }, run.origin_right]
    }

    // ---------------------------------------------------------------- deleting

    /// Merges a delete by events `event..event + len` of the range, of the prepare positions
    /// `pos..pos + len`, removed last to first if `backward`. Calls `removed` with the effect index and
    /// length of each run still there.
    pub fn delete(&mut self, event: EventIndex, pos: u32, len: u32, backward: bool, mut removed: impl FnMut(u32, u32)) {
        let (mut at, mut offset, start) = self.tree.locate(View::Prepare, pos);
        // Where the characters to delete next are in the effect version, if they are still
        // there. Finding the first of them gave it; from there it only moves past what is in
        // the effect version and not in the prepare version.
        let mut index = start + if self.record_run(at).deleted_in_effect { 0 } else { offset };
        let mut done = 0;
        loop {
            // The run holds characters `done..done + n` of those deleted.
            let n = (len - done).min(self.record_run(at).len - offset);
            at = self.isolate(at, offset, n);
            debug_assert_eq!(self.record_run(at).prepare_state, INSERTED);
            // The events that deleted these characters: the next ones, or with a backspace
            // the ones before those already accounted for.
            let first = if backward { event + len - done - n } else { event + done };
            self.note_deleted(DeleteTargets { first_event: first, len: n, record: self.record_run(at).record, backward });
            if self.mark_deleted(at, Some(index)).is_some() {
                removed(index, n);
            }
            done += n;
            if done == len {
                return;
            }
            // The deleted characters carry on into the next run that is in the prepare version.
            at = self.tree.next(at).expect("a delete stays inside the document");
            while self.record_run(at).prepare_state != INSERTED {
                index += self.record_run(at).lengths().effect;
                at = self.tree.next(at).expect("a delete stays inside the document");
            }
            offset = 0;
        }
    }

    /// Records what a stretch of delete events deleted, joining the stretch before when it is
    /// the same delete carrying on.
    fn note_deleted(&mut self, targets: DeleteTargets) {
        let DeleteTargets { first_event: event, len, record, backward } = targets;
        match self.delete_targets.last_mut() {
            // A backspace reaches the next run with the events before those it has used.
            Some(last)
                if last.backward == backward
                    && last.record + last.len == record
                    && (if backward { event + len == last.first_event } else { last.first_event + last.len == event }) =>
            {
                last.len += len;
                last.first_event = last.first_event.min(event);
            }
            _ => self.delete_targets.push(targets),
        }
        self.record_of_event[event as usize..(event + len) as usize].fill(self.delete_targets.len() as u32 - 1);
    }

    /// Merges deletes whose targets are known from [`targets_of`](Self::targets_of).
    pub fn delete_with_known_targets(&mut self, targets: DeleteTargets, mut removed: impl FnMut(u32, u32)) {
        self.note_deleted(targets);
        let (mut record, stop) = (targets.record, targets.record + targets.len);
        while record < stop {
            let (run, offset) = self.locate(record);
            let n = (stop - record).min(self.record_runs[run as usize].len - offset);
            let at = self.isolate(self.tree.cursor(run), offset, n);
            if let Some(index) = self.mark_deleted(at, None) {
                removed(index, n);
            }
            record += n;
        }
    }

    /// Counts one more delete of the run at `at`. Returns its effect index if this is the
    /// first: `index` if given, else found by climbing the tree.
    fn mark_deleted(&mut self, at: Cursor, index: Option<u32>) -> Option<u32> {
        let run = &mut self.record_runs[self.tree.id(at) as usize];
        let (first, before) = (!run.deleted_in_effect, run.lengths());
        run.prepare_state += 1;
        run.deleted_in_effect = true;
        let lengths = run.lengths();
        match index {
            None if first => return Some(self.tree.update(at, lengths)),
            _ if lengths != before => self.tree.set_lengths(at, lengths),
            _ => {}
        }
        index.filter(|_| first)
    }

    /// Returns what the delete events from `event` deleted, up to `end` or until the targets
    /// stop being consecutive.
    pub fn targets_of(&self, event: EventIndex, end: EventIndex) -> DeleteTargets {
        self.delete_targets[self.record_of_event[event as usize] as usize].from(event, end - event)
    }

    // ---------------------------------------------------------------- moving the prepare version

    /// Takes events of the range, all inserts or all deletes, out of the prepare version.
    pub fn retreat(&mut self, kind: OperationKind, events: Range<EventIndex>) {
        self.shift(kind, events, 1u32.wrapping_neg());
    }

    /// Puts events of the range, all inserts or all deletes, back into the prepare version.
    pub fn advance(&mut self, kind: OperationKind, events: Range<EventIndex>) {
        self.shift(kind, events, 1);
    }

    /// Moves events of the range into or out of the prepare version, run by run.
    fn shift(&mut self, kind: OperationKind, events: Range<EventIndex>, delta: u32) {
        let mut event = events.start;
        while event < events.end {
            match kind {
                // Every run, or part of one, that these events created.
                OperationKind::Insert => {
                    let id = self.record_of_event[event as usize];
                    let run = &self.record_runs[id as usize];
                    let (offset, end) = (event - run.record, events.end.min(run.record + run.len));
                    let at = self.isolate(self.tree.cursor(id), offset, end - event);
                    self.shift_record_run(at, delta);
                    event = end;
                }
                OperationKind::Delete => {
                    let DeleteTargets { len: n, mut record, .. } = self.targets_of(event, events.end);
                    let stop = record + n;
                    while record < stop {
                        let (run, offset) = self.locate(record);
                        let len = (stop - record).min(self.record_runs[run as usize].len - offset);
                        let at = self.isolate(self.tree.cursor(run), offset, len);
                        self.shift_record_run(at, delta);
                        record += len;
                    }
                    event += n;
                }
            }
        }
    }

    /// Changes the state of the run at `at` by `delta`, updating its lengths.
    fn shift_record_run(&mut self, at: Cursor, delta: u32) {
        let run = &mut self.record_runs[self.tree.id(at) as usize];
        let before = run.lengths();
        run.prepare_state = run.prepare_state.wrapping_add(delta);
        let lengths = run.lengths();
        if lengths != before {
            self.tree.set_lengths(at, lengths);
        }
    }
}
