mod causal_graph;
mod event_runs;
mod history_range;
mod walk;

pub use causal_graph::CausalGraph;
pub use event_runs::EventRuns;
pub use history_range::HistoryRange;
pub use walk::{walk, WalkStep};

/// An event's number on this replica, in the order it was learned of.
pub type EventIndex = u32;

/// Index of an event run among those the graph holds, in order of their first events. Shifts by
/// one when an earlier run splits.
pub type EventRunIndex = u32;
