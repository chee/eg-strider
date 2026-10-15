use crate::crdt_records::RecordId;
use strider_graph::EventIndex;

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
    pub(crate) fn from(&self, event: EventIndex, len: u32) -> DeleteTargets {
        let (skip, len) = (event - self.first_event, len.min(self.first_event + self.len - event));
        let record = if self.backward { self.record + self.len - skip - len } else { self.record + skip };
        DeleteTargets { first_event: event, len, record, backward: self.backward }
    }
}
