//! eg-strider replays a collaborative text document from its history, in the style of
//! Eg-walker (Gentle and Kleppmann, 2024): events are index-based inserts and deletes with
//! the versions they were made at, and CRDT state is built only where history is
//! concurrent. It adds three things. Regions whose concurrent edits do not touch are merged
//! by offset arithmetic alone, what the CRDT works out is memoised at each critical
//! version, and the text is assembled backwards without a rope.

mod oplog;
mod replay;
mod shift;
mod tracker;

pub use oplog::{AgentId, Edit, Malformed, OpLog, SequenceNumber};
pub use strider_graph::EventId;
