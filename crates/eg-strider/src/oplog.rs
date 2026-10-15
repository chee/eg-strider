mod file;

pub use file::Malformed;

use crate::replay::{self, Base, Memo};
use std::cmp::Ordering;
use std::ops::Range;
use strider_graph::{CausalGraph, EventIndex};

/// How many events a replica made before this one. With the replica, it names an event on every
/// replica.
pub type SequenceNumber = u32;
pub type ReplicaId = u32;

/// Index of an op in the log.
pub(crate) type OperationRunIndex = u32;

/// Where some inserted text is in [`OpLog::inserted_text`]: its first byte, and the byte after
/// its last.
pub(crate) type ByteRange = (u32, u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OperationKind {
    Insert,
    Delete,
}

/// One op of a run handed to [`OpLog::add_remote_ops`]. Positions and lengths count
/// characters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    /// Inserts the next `len` characters of the run's text at `pos`.
    Insert { pos: u32, len: u32 },
    /// Deletes the `len` characters at `pos`, the last of them first if `backspace`.
    Delete { pos: u32, len: u32, backspace: bool },
}

/// A run of consecutive events of one kind, in 16 bytes since a log holds so many.
#[derive(Clone, Copy)]
struct OperationRun {
    first_event: EventIndex,
    /// The number of events, up to [`MAX_OP_LEN`], with [`DELETE`], [`BACKWARD`] and [`DELETES_END_OF_PREVIOUS_INSERT`]
    /// above it.
    len_and_flags: u32,
    /// The positions `pos..pos + len` that the run touches, before its first event.
    pos: u32,
    /// Where the run's inserted text ends in [`OpLog::inserted_text`]. It starts where the text of
    /// the run before ends, and a delete has none.
    inserted_text_end: u32,
}

const _: () = assert!(std::mem::size_of::<OperationRun>() == 16);

/// Marks an op that deletes.
const DELETE: u32 = 1 << 31;
/// Deletes only: marks backspace, whose first event removes the last position.
const BACKWARD: u32 = 1 << 30;
/// Deletes only: marks a delete that removes the end of what the insert before it put in. Found
/// when a run of ops arrives together.
const DELETES_END_OF_PREVIOUS_INSERT: u32 = 1 << 29;
/// The most events one op holds.
const MAX_OP_LEN: u32 = DELETES_END_OF_PREVIOUS_INSERT - 1;

impl OperationRun {
    fn new(
        first_event: EventIndex,
        len: u32,
        kind: OperationKind,
        forward: bool,
        deletes_end_of_previous_insert: bool,
        pos: u32,
        inserted_text_end: u32,
    ) -> OperationRun {
        assert!(len <= MAX_OP_LEN, "an edit longer than an op holds");
        let flags = if kind == OperationKind::Delete { DELETE } else { 0 }
            | if forward { 0 } else { BACKWARD }
            | if deletes_end_of_previous_insert { DELETES_END_OF_PREVIOUS_INSERT } else { 0 };
        OperationRun { first_event, len_and_flags: len | flags, pos, inserted_text_end }
    }

    fn len(&self) -> u32 {
        self.len_and_flags & MAX_OP_LEN
    }

    fn kind(&self) -> OperationKind {
        if self.len_and_flags & DELETE == 0 {
            OperationKind::Insert
        } else {
            OperationKind::Delete
        }
    }

    fn forward(&self) -> bool {
        self.len_and_flags & BACKWARD == 0
    }

    fn deletes_end_of_previous_insert(&self) -> bool {
        self.len_and_flags & DELETES_END_OF_PREVIOUS_INSERT != 0
    }

    /// Takes in `op`, which follows this op, if it carries it on, as typing, holding delete or
    /// holding backspace does. Returns whether it did.
    fn append(&mut self, op: OperationRun) -> bool {
        let (len, kind) = (op.len(), op.kind());
        if self.kind() != kind || self.len() + len > MAX_OP_LEN {
            return false;
        }
        let carries_on = if kind == OperationKind::Insert { self.pos + self.len() == op.pos } else { self.pos == op.pos };
        if self.forward() && op.forward() && !self.deletes_end_of_previous_insert() && carries_on {
            // The flags are above the length, which stays in its bits.
            self.len_and_flags += len;
            self.inserted_text_end = op.inserted_text_end;
            true
        } else if kind == OperationKind::Delete
            && self.deletes_end_of_previous_insert() == op.deletes_end_of_previous_insert()
            && (self.len() == 1 || !self.forward())
            && (len == 1 || !op.forward())
            && op.pos + len == self.pos
        {
            // Backspace: a delete that ends where the last began, each removing one character
            // or already running backwards.
            self.pos = op.pos;
            self.len_and_flags = (self.len_and_flags | BACKWARD) + len;
            true
        } else {
            false
        }
    }
}

/// A run of consecutive events by one replica.
struct ReplicaRun {
    first_event: EventIndex,
    len: u32,
    replica: ReplicaId,
    sequence_number: SequenceNumber,
}

/// Part of an op, as the replay sees it.
#[derive(Clone, Copy)]
pub(crate) struct OperationRunSlice {
    pub first_event: EventIndex,
    pub len: u32,
    pub kind: OperationKind,
    pub forward: bool,
    /// Whether this is a whole delete that trims the insert before it.
    pub deletes_end_of_previous_insert: bool,
    /// Inserts go at `pos`; deletes remove `pos..pos + len`.
    pub pos: u32,
    /// Inserts only: the inserted text.
    pub text: ByteRange,
}

/// The operation log: every insert and delete this replica knows of, in the order it learned of
/// them, with each event's parents, author and position. Consecutive events are run-length
/// encoded into ops, and inserted text lives in one append-only buffer. A checkout derives the
/// document in the style of Eg-walker (Gentle and Kleppmann, 2024), building CRDT state only
/// where history is concurrent; on top of that, regions whose concurrent edits do not touch are
/// merged by shifting positions, what the CRDT works out is memoised at critical versions, and
/// the text is built backwards without a rope.
///
/// Events must be valid as the Eg-walker paper defines it (Definition C.1): each is added once,
/// its parents are events the log already has, and each position exists in the document at its
/// parents. The paper's replicas are not Byzantine, so a run of ops also carries exactly the text
/// its inserts put in. None of this is checked.
#[derive(Default)]
pub struct OpLog {
    pub(crate) graph: CausalGraph,
    ops: Vec<OperationRun>,
    inserted_text: String,
    replica_names: Vec<String>,
    next_sequence_numbers: Vec<SequenceNumber>,
    replica_runs: Vec<ReplicaRun>,
    memo: Memo,
    /// The document as of some first events, which the next merge starts from.
    base: Base,
    /// The fully merged document, when the log ends with concurrent events. The base stays put
    /// then, since moving it into events still being added would cut them up at every merge.
    latest: Option<Base>,
    /// History from before the first event held here, from a loaded file, still unread.
    packed: Option<file::Packed>,
    /// Scratch for a new event's parents.
    parents: Vec<EventIndex>,
    /// The characters the inserts put in, and the characters the deletes take out.
    totals: (u32, u32),
}

/// Counts the edits at the start of `edits`, up to `room`, that go on holding backspace from a
/// delete at `pos`: each removing the one character before the one before.
#[inline(never)]
fn held_backspaces(edits: &[Operation], pos: u32, room: u32) -> u32 {
    let mut held = 0;
    while held < room && matches!(edits.get(held as usize), Some(&Operation::Delete { pos: at, len: 1, .. }) if at + held + 1 == pos) {
        held += 1;
    }
    held
}

/// Returns how many bytes of `text` come before the first that is not ASCII.
fn ascii_prefix(text: &[u8]) -> usize {
    // Whole blocks are checked a word at a time; only the block that fails is searched.
    let blocks = text.chunks(256).take_while(|block| block.is_ascii()).count();
    let rest = &text[(blocks * 256).min(text.len())..];
    text.len() - rest.len() + rest.iter().take_while(|byte| byte.is_ascii()).count()
}

impl OpLog {
    /// Creates an empty log.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the number of events: characters inserted plus characters deleted.
    pub fn len(&self) -> usize {
        self.graph.len() as usize
    }

    /// Tells whether the log has no events.
    pub fn is_empty(&self) -> bool {
        self.graph.is_empty()
    }

    /// Returns the id of the replica called `name`, registering it if it is new.
    pub fn get_or_create_replica(&mut self, name: &str) -> ReplicaId {
        if let Some(i) = self.replica_names.iter().position(|n| n == name) {
            return i as ReplicaId;
        }
        self.replica_names.push(name.to_string());
        self.next_sequence_numbers.push(0);
        (self.replica_names.len() - 1) as ReplicaId
    }

    /// Returns a replica's name, if the log has the replica.
    pub fn replica_name(&self, replica: ReplicaId) -> Option<&str> {
        self.replica_names.get(replica as usize).map(String::as_str)
    }

    /// Returns the events with no children: the version a local edit builds on.
    pub fn frontier(&self) -> Vec<EventIndex> {
        self.graph.frontier().to_vec()
    }

    /// Merges every event and returns the document, going through only the events since the
    /// last merge where history allows.
    pub fn checkout(&mut self) -> &str {
        let len = self.graph.len();
        if self.base.events != len && self.latest.as_ref().is_none_or(|latest| latest.events != len) {
            // An event made without all that the base holds is concurrent with some of it.
            // The merge then has to tell what the base holds from the rest.
            if self.graph.start_run_at(self.base.events) {
                self.split_op(self.base.events);
                self.memo.outdate();
            }
            let mut memo = std::mem::take(&mut self.memo);
            let (text, chars) = replay::checkout(self, &mut memo, &self.base);
            self.memo = memo;
            let merged = Base { events: len, chars, text };
            if self.graph.frontier().len() == 1 {
                (self.base, self.latest) = (merged, None);
            } else {
                self.latest = Some(merged);
            }
        }
        &self.merged().text
    }

    /// Returns the document as of the last merge.
    fn merged(&self) -> &Base {
        self.latest.as_ref().unwrap_or(&self.base)
    }

    /// Returns how many events of a loaded log's history are still unread.
    pub fn unread_events(&self) -> usize {
        self.graph.first_held_event() as usize
    }

    /// Returns how many regions the last checkout could not merge by shifting alone.
    pub fn ranges_using_crdt_records(&self) -> usize {
        self.memo.ranges_using_crdt_records()
    }

    /// Drops what earlier checkouts worked out, so that the next one starts from scratch. This
    /// reads back all saved history, which fails if it is damaged.
    pub fn clear_replay_cache(&mut self) -> Result<(), Malformed> {
        self.unpack_from(0)?;
        (self.memo, self.base, self.latest) = (Memo::default(), Base::default(), None);
        Ok(())
    }

    /// Inserts `text` at `pos` in the document as of the current frontier, which never needs
    /// saved history read back.
    pub fn add_insert(&mut self, replica: ReplicaId, pos: usize, text: &str) -> Range<EventIndex> {
        self.append_events(replica, None, None, OperationKind::Insert, true, pos, text.chars().count(), text)
    }

    /// Deletes `range` from the document as of the current frontier.
    pub fn add_delete(&mut self, replica: ReplicaId, range: Range<usize>) -> Range<EventIndex> {
        self.append_events(replica, None, None, OperationKind::Delete, true, range.start, range.len(), "")
    }

    /// Inserts `text` at `pos` in the document as of version `parents`, which must be ascending.
    /// Fails only if saved history that this needs read back is damaged.
    pub fn add_insert_at(&mut self, replica: ReplicaId, parents: &[EventIndex], pos: usize, text: &str) -> Result<Range<EventIndex>, Malformed> {
        self.unpack_needed_history(parents)?;
        Ok(self.append_events(replica, None, Some(parents), OperationKind::Insert, true, pos, text.chars().count(), text))
    }

    /// Deletes `range` from the document as of version `parents`, which must be ascending.
    /// `forward` is false for backspace, which removes the last character first. Fails only if
    /// saved history that this needs read back is damaged.
    pub fn add_delete_at(&mut self, replica: ReplicaId, parents: &[EventIndex], range: Range<usize>, forward: bool) -> Result<Range<EventIndex>, Malformed> {
        self.unpack_needed_history(parents)?;
        Ok(self.append_events(replica, None, Some(parents), OperationKind::Delete, forward, range.start, range.len(), ""))
    }

    /// As [`add_insert_at`](Self::add_insert_at), for an event from elsewhere with a fixed
    /// sequence number.
    pub fn add_remote_insert(
        &mut self,
        replica: ReplicaId,
        sequence_number: SequenceNumber,
        parents: &[EventIndex],
        pos: usize,
        text: &str,
    ) -> Result<Range<EventIndex>, Malformed> {
        self.unpack_needed_history(parents)?;
        Ok(self.append_events(replica, Some(sequence_number), Some(parents), OperationKind::Insert, true, pos, text.chars().count(), text))
    }

    /// As [`add_delete_at`](Self::add_delete_at), for an event from elsewhere with a fixed
    /// sequence number.
    pub fn add_remote_delete(
        &mut self,
        replica: ReplicaId,
        sequence_number: SequenceNumber,
        parents: &[EventIndex],
        range: Range<usize>,
        forward: bool,
    ) -> Result<Range<EventIndex>, Malformed> {
        self.unpack_needed_history(parents)?;
        Ok(self.append_events(replica, Some(sequence_number), Some(parents), OperationKind::Delete, forward, range.start, range.len(), ""))
    }

    /// Appends a run of ops by one replica from elsewhere: the first made at `parents`, which must
    /// be ascending, each other following the one before. `text` holds the inserts' text, in
    /// order. Fails only if saved history that this needs read back is damaged.
    pub fn add_remote_ops(
        &mut self,
        replica: ReplicaId,
        sequence_number: SequenceNumber,
        parents: &[EventIndex],
        ops: &[Operation],
        text: &str,
    ) -> Result<Range<EventIndex>, Malformed> {
        self.unpack_needed_history(parents)?;
        let event = self.graph.len();
        let total = self.append_operations(event, parents, ops, text);
        if total == 0 {
            return Ok(event..event);
        }
        let (_, events) = self.add_to_causal_graph(Some(parents), total);
        self.assign_replica(replica, Some(sequence_number), event, total);
        Ok(events)
    }

    /// Stores a run's ops and text, and returns the number of events. The graph and the replica's
    /// numbering are left to the caller. Never inlined: with its callers' values also live, its
    /// loop runs out of registers and keeps the op being built in memory, which costs a tenth
    /// more on a run of many edits.
    #[inline(never)]
    fn append_operations(&mut self, event: EventIndex, parents: &[EventIndex], ops: &[Operation], text: &str) -> u32 {
        let parts = |edit: &Operation| match *edit {
            Operation::Insert { pos, len } => (OperationKind::Insert, true, pos, len),
            Operation::Delete { pos, len, backspace } => (OperationKind::Delete, !backspace, pos, len),
        };
        // The op being built, which each edit that carries it on is joined into. It is stored
        // once one does not, so that most edits change nothing in memory.
        let mut last = if event > 0 && parents == [event - 1] { self.ops.pop() } else { None };
        let mut inserted = 0;
        let mut at = self.inserted_text.len() as u32;
        self.inserted_text.push_str(text);
        let end = u32::try_from(self.inserted_text.len()).expect("more inserted text than a u32 counts");
        // Characters and bytes coincide in ASCII text. `plain` is where the text stops being
        // ASCII: found again only once an insert reaches it, since such characters are few.
        let ascii_until = |from: u32| from + ascii_prefix(&text.as_bytes()[(from - (end - text.len() as u32)) as usize..]) as u32;
        let mut plain = ascii_until(at);
        // The insert just before, if only deletes of its end have followed: where what is
        // left of it ends, and how much is left.
        let (mut next, mut open) = (event, None::<(u32, u32)>);
        let mut i = 0;
        while let Some(edit) = ops.get(i) {
            i += 1;
            let (kind, mut forward, mut pos, mut len) = parts(edit);
            if len == 0 {
                continue;
            }
            // Holding backspace: deletes of one character, each just before the one before,
            // often hundreds in a row. They are taken as one delete that runs backwards, no
            // longer than what is left of the insert before if the first trims it.
            if kind == OperationKind::Delete && len == 1 && matches!(ops.get(i), Some(&Operation::Delete { pos: at, len: 1, .. }) if at + 1 == pos) {
                // Rare in most histories, and laid out away from the rest so that it costs
                // them nothing much; where it is common, a whole run is taken at once.
                std::hint::cold_path();
                let room = match open {
                    Some((end, left)) if pos + 1 == end && left >= 1 => left,
                    _ => MAX_OP_LEN,
                };
                let held = held_backspaces(&ops[i..], pos, room - 1);
                if held > 0 {
                    (forward, pos, len) = (false, pos - held, len + held);
                    i += held as usize;
                }
            }
            let bytes = match kind {
                OperationKind::Delete => 0,
                OperationKind::Insert if len <= plain.saturating_sub(at) => len,
                OperationKind::Insert => {
                    let bytes = self.prefix((at, end), len);
                    plain = ascii_until(at + bytes);
                    bytes
                }
            };
            let deletes_end_of_previous_insert = kind == OperationKind::Delete && open.is_some_and(|(end, left)| pos + len == end && len <= left);
            let op = OperationRun::new(next, len, kind, forward, deletes_end_of_previous_insert, pos, at + bytes);
            if kind == OperationKind::Insert {
                inserted += len;
            }
            if !last.as_mut().is_some_and(|last| last.append(op)) {
                self.ops.extend(last.replace(op));
            }
            let whole = last.map_or(len, |last| last.len());
            open = match kind {
                OperationKind::Insert => Some((pos + len, whole)),
                OperationKind::Delete if deletes_end_of_previous_insert => open.map(|(end, left)| (end - len, left - len)),
                OperationKind::Delete => None,
            };
            (at, next) = (at + bytes, next.checked_add(len).expect("more events than a u32 counts"));
        }
        self.ops.extend(last);
        // Every event either inserts or removes a character.
        self.totals = (self.totals.0 + inserted, self.totals.1 + (next - event - inserted));
        debug_assert_eq!(at, end, "the text is not what the inserts put in");
        next - event
    }

    /// Appends a run of events of one kind, extending the last op when it simply carries on.
    #[allow(clippy::too_many_arguments)]
    fn append_events(
        &mut self,
        replica: ReplicaId,
        sequence_number: Option<SequenceNumber>,
        parents: Option<&[EventIndex]>,
        kind: OperationKind,
        forward: bool,
        pos: usize,
        len: usize,
        text: &str,
    ) -> Range<EventIndex> {
        let event = self.graph.len();
        if len == 0 {
            return event..event;
        }
        let len = u32::try_from(len).expect("more events than a u32 counts");
        let pos = u32::try_from(pos).expect("a position past what a u32 counts");
        let (continues, events) = self.add_to_causal_graph(parents, len);
        self.assign_replica(replica, sequence_number, event, len);
        self.inserted_text.push_str(text);
        let end = u32::try_from(self.inserted_text.len()).expect("more inserted text than a u32 counts");
        self.append_operation_run(continues, OperationRun::new(event, len, kind, forward, false, pos, end));
        events
    }

    /// Reads back the saved history that an event made at `parents` needs, if it was made
    /// without all of it: with no parent held here, and not all of the old frontier for parents.
    /// A saved block starts at a critical version, so the event has all that comes before the
    /// block holding its latest parent.
    fn unpack_needed_history(&mut self, parents: &[EventIndex]) -> Result<(), Malformed> {
        if let Some(packed) = &self.packed {
            let floor = self.graph.first_held_event();
            if !parents.iter().any(|&parent| parent >= floor) && !packed.frontier.iter().all(|event| parents.contains(event)) {
                let block = parents.iter().max().map_or(0, |&parent| packed.block_holding(parent));
                self.unpack_from(block)?;
            }
        }
        Ok(())
    }

    /// Adds a chain of `len` events to the graph, the first at `parents` or else the frontier.
    /// Returns whether they follow the last event, and their ids.
    fn add_to_causal_graph(&mut self, parents: Option<&[EventIndex]>, len: u32) -> (bool, Range<EventIndex>) {
        let event = self.graph.len();
        // Most events follow the last one, whose number is then all the graph needs.
        let continues = event > 0 && parents.map_or(self.graph.frontier() == [event - 1], |parents| parents == [event - 1]);
        if continues {
            return (true, self.graph.push(&[event - 1], len));
        }
        let mut at = std::mem::take(&mut self.parents);
        at.clear();
        match parents {
            Some(parents) => at.extend_from_slice(parents),
            None => at.extend_from_slice(self.graph.frontier()),
        }
        let events = self.graph.push(&at, len);
        self.parents = at;
        (false, events)
    }

    /// Records that `replica` made the `len` events from `event`, numbered from `sequence_number`
    /// or else from where its last left off.
    fn assign_replica(&mut self, replica: ReplicaId, sequence_number: Option<SequenceNumber>, event: EventIndex, len: u32) {
        let sequence_number = sequence_number.unwrap_or(self.next_sequence_numbers[replica as usize]);
        let next = &mut self.next_sequence_numbers[replica as usize];
        *next = (*next).max(sequence_number + len);
        match self.replica_runs.last_mut() {
            Some(r) if r.replica == replica && r.first_event + r.len == event && r.sequence_number + r.len == sequence_number => r.len += len,
            _ => self.replica_runs.push(ReplicaRun { first_event: event, len, replica, sequence_number }),
        }
    }

    /// Stores an op, joining it into the last op when it follows and carries it on.
    fn append_operation_run(&mut self, follows: bool, op: OperationRun) {
        match op.kind() {
            OperationKind::Insert => self.totals.0 += op.len(),
            OperationKind::Delete => self.totals.1 += op.len(),
        }
        if !(follows && self.ops.last_mut().is_some_and(|last| last.append(op))) {
            self.ops.push(op);
        }
    }

    /// Makes an op start at event `at`, if one runs across it.
    fn split_op(&mut self, at: EventIndex) {
        let i = self.op_containing(at) as usize;
        let Some(op) = self.ops.get(i).filter(|op| op.first_event < at) else { return };
        let (start, end) = (op.first_event, op.first_event + op.len());
        let part = |events: Range<EventIndex>| {
            let part = self.slice(i, &events);
            OperationRun::new(part.first_event, part.len, part.kind, part.forward, false, part.pos, part.text.1)
        };
        let parts = [part(start..at), part(at..end)];
        self.ops.splice(i..=i, parts);
        // A delete only trims straight after an insert or after another delete that does.
        for after in self.ops[i + 2..].iter_mut().take_while(|after| after.deletes_end_of_previous_insert()) {
            after.len_and_flags &= !DELETES_END_OF_PREVIOUS_INSERT;
        }
    }

    /// Returns how many bytes the first `len` characters of the text at `bytes` take.
    #[cold]
    fn prefix(&self, bytes: ByteRange, len: u32) -> u32 {
        let text = self.text(bytes);
        match text.char_indices().nth(len as usize) {
            Some((bytes, _)) => bytes as u32,
            None => {
                debug_assert_eq!(text.chars().count(), len as usize, "the text is not what the inserts put in");
                text.len() as u32
            }
        }
    }

    /// Returns the text at `bytes` without its last `cut` characters: what a
    /// [trimming](OperationRun::deletes_end_of_previous_insert) delete leaves of the insert.
    pub(crate) fn trimmed(&self, bytes: ByteRange, len: u32, cut: u32) -> ByteRange {
        if bytes.1 - bytes.0 == len {
            return (bytes.0, bytes.1 - cut);
        }
        (bytes.0, bytes.0 + self.prefix(bytes, len - cut))
    }

    /// Returns the replica that made `event`, and its sequence number.
    fn replica_at(&self, event: EventIndex) -> (ReplicaId, SequenceNumber) {
        let run = &self.replica_runs[self.replica_runs.partition_point(|r| r.first_event <= event) - 1];
        (run.replica, run.sequence_number + (event - run.first_event))
    }

    /// Breaks a tie between concurrent inserts at the same place: does the insert from `a` go
    /// before the one from `b`?
    pub(crate) fn inserts_before(&self, a: EventIndex, b: EventIndex) -> bool {
        let (replica_a, sequence_number_a) = self.replica_at(a);
        let (replica_b, sequence_number_b) = self.replica_at(b);
        match self.replica_names[replica_a as usize].cmp(&self.replica_names[replica_b as usize]) {
            Ordering::Less => true,
            Ordering::Greater => false,
            Ordering::Equal => sequence_number_a < sequence_number_b,
        }
    }

    /// Returns, for each event in ascending `starts`, the index of the op holding it, or the
    /// number of ops for the event after the last.
    pub(crate) fn ops_containing(&self, starts: impl Iterator<Item = EventIndex>) -> Vec<OperationRunIndex> {
        let mut op = 0;
        starts
            .map(|event| {
                while self.ops.get(op).is_some_and(|op| op.first_event + op.len() <= event) {
                    op += 1;
                }
                op as OperationRunIndex
            })
            .collect()
    }

    /// Iterates over the parts of ops inside `events`. `ops` runs from the op holding the first
    /// event to the op holding the event after the last.
    pub(crate) fn ops(&self, ops: Range<OperationRunIndex>, events: Range<EventIndex>) -> impl DoubleEndedIterator<Item = OperationRunSlice> + '_ {
        // The op containing the event after the last holds some of these too if it starts
        // before then.
        let straddles = self.ops.get(ops.end as usize).is_some_and(|op| op.first_event < events.end);
        let count = if events.is_empty() { 0 } else { (ops.end - ops.start) as usize + straddles as usize };
        (ops.start as usize..ops.start as usize + count).map(move |op| self.slice(op, &events))
    }

    /// Returns the index of the op holding `event`.
    pub(crate) fn op_containing(&self, event: EventIndex) -> OperationRunIndex {
        self.ops.partition_point(|op| op.first_event + op.len() <= event) as OperationRunIndex
    }

    /// Returns all inserted text, which [`OperationRunSlice`] ranges point into.
    pub(crate) fn inserted_text(&self) -> &str {
        &self.inserted_text
    }

    /// Returns the inserted text at a range.
    fn text(&self, (start, end): ByteRange) -> &str {
        &self.inserted_text[start as usize..end as usize]
    }

    /// Returns what [`lengths`](Self::lengths) would count over every event.
    pub(crate) fn total_lengths(&self) -> (u32, u32) {
        self.totals
    }

    /// Counts the characters `events` insert and remove, applied as recorded. `first_op` is the
    /// op holding the first of them.
    pub(crate) fn lengths(&self, first_op: OperationRunIndex, events: Range<EventIndex>) -> (u32, u32) {
        let (mut inserted, mut removed) = (0, 0);
        for op in &self.ops[first_op as usize..] {
            if op.first_event.max(events.start) >= events.end {
                break;
            }
            let len = events.end.min(op.first_event + op.len()) - events.start.max(op.first_event);
            // Which kind an op is cannot be predicted, so both sums are added to without
            // branching on it.
            let insert = (op.kind() == OperationKind::Insert) as u32;
            inserted += len * insert;
            removed += len * (1 - insert);
        }
        (inserted, removed)
    }

    /// Returns the bytes of characters `from..to` of the text at `bytes`.
    #[cold]
    fn char_bytes(&self, bytes: ByteRange, from: u32, to: u32) -> ByteRange {
        let text = self.text(bytes);
        // Where each character starts, then where the text ends.
        let mut starts = text.char_indices().map(|(byte, _)| byte as u32).chain([text.len() as u32]);
        let first = starts.nth(from as usize).expect("an op is cut within its text");
        let last = if from < to { starts.nth((to - from - 1) as usize).expect("an op is cut within its text") } else { first };
        (bytes.0 + first, bytes.0 + last)
    }

    /// Cuts out the part of op `i` inside `events`.
    #[inline]
    fn slice(&self, i: usize, events: &Range<EventIndex>) -> OperationRunSlice {
        let op = &self.ops[i];
        let (len, kind, forward) = (op.len(), op.kind(), op.forward());
        let bytes = (if i == 0 { 0 } else { self.ops[i - 1].inserted_text_end }, op.inserted_text_end);
        let from = events.start.max(op.first_event) - op.first_event;
        let to = events.end.min(op.first_event + len) - op.first_event;
        // Nearly always the whole op, which needs no working out whatever its kind.
        if to - from == len {
            return OperationRunSlice {
                first_event: op.first_event,
                len,
                kind,
                forward,
                deletes_end_of_previous_insert: op.deletes_end_of_previous_insert(),
                pos: op.pos,
                text: bytes,
            };
        }
        let (pos, text) = match (kind, forward) {
            (OperationKind::Insert, _) => {
                let (start, end) = bytes;
                // Characters and bytes coincide when the text is all ASCII.
                let text = if end - start == len { (start + from, start + to) } else { self.char_bytes(bytes, from, to) };
                (op.pos + from, text)
            }
            // Each forward delete lands on the same position, so a later slice starts where
            // the op did.
            (OperationKind::Delete, true) => (op.pos, bytes),
            (OperationKind::Delete, false) => (op.pos + len - to, bytes),
        };
        OperationRunSlice { first_event: op.first_event + from, len: to - from, kind, forward, deletes_end_of_previous_insert: false, pos, text }
    }
}
