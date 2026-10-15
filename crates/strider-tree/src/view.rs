/// Which version of the document a length or index counts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum View {
    /// The document as the merged event's author saw it.
    Prepare,
    /// The document as merged so far.
    Effect,
}
