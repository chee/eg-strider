/// In place of a final position: these characters do not survive.
pub(crate) const DELETED: u32 = u32::MAX;

#[derive(Clone, Copy)]
pub(crate) struct Piece {
    pub(crate) len: u32,
    /// Where the piece's first character is in the final text, or [`DELETED`].
    pub(crate) at: u32,
}

impl Piece {
    /// Returns the piece with its length changed to `len`.
    pub(crate) fn with_len(self, len: u32) -> Piece {
        Piece { len, ..self }
    }

    /// Returns the piece without its first `skip` characters.
    pub(crate) fn after(self, skip: u32) -> Piece {
        Piece { len: self.len - skip, at: if self.at == DELETED { DELETED } else { self.at + skip } }
    }

    /// Tells whether `next`, which follows this piece, shares its fate: both deleted, or both
    /// surviving side by side.
    pub(crate) fn continues_into(self, next: Piece) -> bool {
        if self.at == DELETED || next.at == DELETED {
            self.at == next.at
        } else {
            self.at + self.len == next.at
        }
    }
}
