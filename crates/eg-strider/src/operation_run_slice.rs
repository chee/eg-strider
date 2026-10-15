use crate::operation_kind::OperationKind;
use crate::oplog::ByteRange;
use strider_graph::EventIndex;

/// Part of an op, as the replay sees it.
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
