#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OperationKind {
    Insert,
    Delete,
}
