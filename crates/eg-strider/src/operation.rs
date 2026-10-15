use strider_graph::EventIndex;

/// An insert or delete handed to [`OpLog::add_remote_ops`](crate::OpLog::add_remote_ops), with
/// positions in characters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    /// Inserts the next `len` characters of the chain's text at `pos`.
    Insert { pos: u32, len: u32 },
    /// Deletes the `len` characters at `pos`, the last first if `backward`.
    Delete { pos: u32, len: u32, backward: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OperationKind {
    Insert,
    Delete,
}

/// Consecutive events forming one insert or delete, packed into 16 bytes.
#[derive(Clone, Copy)]
pub(crate) struct OperationRun {
    pub(crate) first_event: EventIndex,
    /// The number of events, up to [`MAX_OP_LEN`], with [`DELETE`], [`BACKWARD`] and
    /// [`DELETES_END_OF_PREVIOUS_INSERT`] above it.
    pub(crate) len_and_flags: u32,
    /// The positions `pos..pos + len` that the run touches, before its first event.
    pub(crate) pos: u32,
    /// Where the run's inserted text ends in
    /// [`OpLog::inserted_text`](crate::OpLog::inserted_text). It starts where the text of the run
    /// before ends, and a delete has none.
    pub(crate) inserted_text_end: u32,
}

const _: () = assert!(std::mem::size_of::<OperationRun>() == 16);

const DELETE: u32 = 1 << 31;
/// Deletes only: the first event removes the last position, as backspace does.
const BACKWARD: u32 = 1 << 30;
/// Deletes only: the delete removes the end of the text that the insert before it put in.
pub(crate) const DELETES_END_OF_PREVIOUS_INSERT: u32 = 1 << 29;
/// The most events one op holds.
pub(crate) const MAX_OP_LEN: u32 = DELETES_END_OF_PREVIOUS_INSERT - 1;

impl OperationRun {
    pub(crate) fn new(
        first_event: EventIndex,
        len: u32,
        kind: OperationKind,
        backward: bool,
        deletes_end_of_previous_insert: bool,
        pos: u32,
        inserted_text_end: u32,
    ) -> OperationRun {
        assert!(len <= MAX_OP_LEN, "an edit longer than an op holds");
        let flags = if kind == OperationKind::Delete { DELETE } else { 0 }
            | if backward { BACKWARD } else { 0 }
            | if deletes_end_of_previous_insert { DELETES_END_OF_PREVIOUS_INSERT } else { 0 };
        OperationRun { first_event, len_and_flags: len | flags, pos, inserted_text_end }
    }

    pub(crate) fn len(&self) -> u32 {
        self.len_and_flags & MAX_OP_LEN
    }

    pub(crate) fn kind(&self) -> OperationKind {
        if self.len_and_flags & DELETE == 0 {
            OperationKind::Insert
        } else {
            OperationKind::Delete
        }
    }

    pub(crate) fn backward(&self) -> bool {
        self.len_and_flags & BACKWARD != 0
    }

    pub(crate) fn deletes_end_of_previous_insert(&self) -> bool {
        self.len_and_flags & DELETES_END_OF_PREVIOUS_INSERT != 0
    }

    /// Joins `op`, which follows this op, onto it if it continues it as typing, delete or backspace
    /// would, returning whether it did.
    pub(crate) fn append(&mut self, op: OperationRun) -> bool {
        let (len, kind) = (op.len(), op.kind());
        if self.kind() != kind || self.len() + len > MAX_OP_LEN {
            return false;
        }
        let carries_on = if kind == OperationKind::Insert { self.pos + self.len() == op.pos } else { self.pos == op.pos };
        if !self.backward() && !op.backward() && !self.deletes_end_of_previous_insert() && carries_on {
            // The flags are above the length, which stays in its bits.
            self.len_and_flags += len;
            self.inserted_text_end = op.inserted_text_end;
            true
        } else if kind == OperationKind::Delete
            && self.deletes_end_of_previous_insert() == op.deletes_end_of_previous_insert()
            && (self.len() == 1 || self.backward())
            && (len == 1 || op.backward())
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

/// A part of an operation run, unpacked.
#[derive(Clone, Copy)]
pub(crate) struct OperationRunSlice {
    pub first_event: EventIndex,
    pub len: u32,
    pub kind: OperationKind,
    pub backward: bool,
    /// Whether this is a whole delete that trims the insert before it.
    pub deletes_end_of_previous_insert: bool,
    /// Inserts go at `pos`; deletes remove `pos..pos + len`.
    pub pos: u32,
    /// Inserts only: the inserted text.
    pub text: ByteRange,
}

/// A byte range of [`OpLog::inserted_text`](crate::OpLog::inserted_text).
pub(crate) type ByteRange = (u32, u32);
