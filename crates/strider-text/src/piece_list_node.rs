use crate::piece::Piece;

/// Pieces per node. Tiny in debug builds, so that tests split and empty nodes often.
pub(crate) const PIECES_PER_NODE: usize = if cfg!(debug_assertions) { 4 } else { 32 };

/// Up to [`PIECES_PER_NODE`] pieces, in document order.
#[derive(Clone, Copy)]
pub(crate) struct PieceListNode {
    pub(crate) len: usize,
    /// The finger: the piece found last, and the characters before it in the node. The next
    /// lookup usually lands near it, on either side.
    pub(crate) finger: (usize, u32),
    pub(crate) pieces: [Piece; PIECES_PER_NODE],
}

impl PieceListNode {
    pub(crate) const EMPTY: PieceListNode = PieceListNode { len: 0, finger: (0, 0), pieces: [Piece { len: 0, at: 0 }; PIECES_PER_NODE] };

    /// Returns the number of pieces.
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Returns piece `i`.
    pub(crate) fn piece(&self, i: usize) -> Piece {
        self.pieces[i]
    }

    /// Notes that piece `i` is changing, which moves any remembered piece after it.
    pub(crate) fn touch(&mut self, i: usize) {
        if i < self.finger.0 {
            self.finger = (0, 0);
        }
    }

    /// Replaces piece `i`.
    pub(crate) fn set(&mut self, i: usize, piece: Piece) {
        self.touch(i);
        self.pieces[i] = piece;
    }

    /// Finds the piece holding character `offset` of the node, and the offset into it. One
    /// past the last piece at the node's end.
    pub(crate) fn find(&mut self, offset: u32) -> (usize, u32) {
        let (mut i, mut start) = self.finger;
        while offset < start {
            i -= 1;
            start -= self.pieces[i].len;
        }
        while i < self.len && offset >= start + self.pieces[i].len {
            start += self.pieces[i].len;
            i += 1;
        }
        self.finger = (i, start);
        (i, offset - start)
    }

    /// Inserts a piece at `i`. There must be room.
    pub(crate) fn insert(&mut self, i: usize, piece: Piece) {
        self.touch(i);
        self.pieces.copy_within(i..self.len, i + 1);
        self.pieces[i] = piece;
        self.len += 1;
    }

    /// Removes piece `i`.
    pub(crate) fn remove(&mut self, i: usize) {
        self.touch(i);
        self.pieces.copy_within(i + 1..self.len, i);
        self.len -= 1;
    }
}
