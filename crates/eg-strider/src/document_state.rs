use strider_graph::EventIndex;

/// The document state with the first `events` events merged, which a replay of the rest starts
/// from.
#[derive(Default)]
pub(crate) struct DocumentState {
    pub events: EventIndex,
    /// The length of `text` in characters.
    pub chars: u32,
    pub text: String,
}
