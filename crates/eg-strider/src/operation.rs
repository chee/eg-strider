/// One op of a run handed to [`OpLog::add_remote_ops`](crate::OpLog::add_remote_ops).
/// Positions and lengths count characters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    /// Inserts the next `len` characters of the run's text at `pos`.
    Insert { pos: u32, len: u32 },
    /// Deletes the `len` characters at `pos`, the last of them first if `backward`, as backspace
    /// does.
    Delete { pos: u32, len: u32, backward: bool },
}
