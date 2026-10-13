//! The piece table behind [`Rewind`](crate::Rewind). A piece is a stretch of the document
//! that either sits at a known offset of the final text or is deleted: some later edit
//! removes it. Pieces are kept in an unrolled list: chunks of up to 32 under a flat list of
//! chunk lengths. Edits cluster, so the chunk used last, and the piece found last in it, are
//! tried first.

/// In place of a final position: these characters do not survive.
const DELETED: u32 = u32::MAX;

/// PieceTable per chunk. Tiny in debug builds, so that tests split and empty chunks often.
const CHUNK: usize = if cfg!(debug_assertions) { 4 } else { 32 };

#[derive(Clone, Copy)]
struct Piece {
    len: u32,
    /// Where the piece's first character is in the final text, or [`DELETED`].
    at: u32,
}

impl Piece {
    /// Returns the piece with its length changed to `len`.
    fn with_len(self, len: u32) -> Piece {
        Piece { len, ..self }
    }

    /// Returns the piece without its first `skip` characters.
    fn after(self, skip: u32) -> Piece {
        Piece { len: self.len - skip, at: if self.at == DELETED { DELETED } else { self.at + skip } }
    }

    /// Tells whether `next`, which follows this piece in the document, shares its fate: both
    /// deleted, or both surviving next to each other.
    fn continues_into(self, next: Piece) -> bool {
        if self.at == DELETED || next.at == DELETED {
            self.at == next.at
        } else {
            self.at + self.len == next.at
        }
    }
}

/// Up to [`CHUNK`] pieces, in document order.
#[derive(Clone, Copy)]
struct Chunk {
    len: usize,
    /// The piece found last, and how many characters come before it in the chunk: the next
    /// lookup usually lands on or just after it.
    last: (usize, u32),
    pieces: [Piece; CHUNK],
}

impl Chunk {
    const EMPTY: Chunk = Chunk { len: 0, last: (0, 0), pieces: [Piece { len: 0, at: 0 }; CHUNK] };

    /// Returns how many pieces the chunk holds.
    fn len(&self) -> usize {
        self.len
    }

    /// Returns piece `i`.
    fn piece(&self, i: usize) -> Piece {
        self.pieces[i]
    }

    /// Notes that piece `i` is changing: a remembered piece after it no longer starts where
    /// it did.
    fn touch(&mut self, i: usize) {
        if i < self.last.0 {
            self.last = (0, 0);
        }
    }

    /// Replaces piece `i`.
    fn set(&mut self, i: usize, piece: Piece) {
        self.touch(i);
        self.pieces[i] = piece;
    }

    /// Finds the piece holding the character `offset` characters into the chunk, and how far into
    /// that piece it is. One past the last piece for the end of the chunk.
    fn find(&mut self, offset: u32) -> (usize, u32) {
        let (mut i, mut start) = if offset < self.last.1 { (0, 0) } else { self.last };
        while i < self.len && offset >= start + self.pieces[i].len {
            start += self.pieces[i].len;
            i += 1;
        }
        self.last = (i, start);
        (i, offset - start)
    }

    /// Adds a piece as piece `i`. There must be room.
    fn insert(&mut self, i: usize, piece: Piece) {
        self.touch(i);
        self.pieces.copy_within(i..self.len, i + 1);
        self.pieces[i] = piece;
        self.len += 1;
    }

    /// Removes piece `i`.
    fn remove(&mut self, i: usize) {
        self.touch(i);
        self.pieces.copy_within(i + 1..self.len, i);
        self.len -= 1;
    }
}

pub(crate) struct PieceTable {
    chunks: Vec<Chunk>,
    /// Chunks no longer in use.
    spare: Vec<u32>,
    /// The chunks in document order, and how many characters each holds.
    order: Vec<u32>,
    totals: Vec<u32>,
    /// The chunk used last: its place in `order`, and the position where it starts.
    near: (usize, u32),
}

impl PieceTable {
    /// Creates a document of `len` characters, all of which survive: the final text itself.
    pub fn new(len: u32) -> Self {
        let mut first = Chunk::EMPTY;
        if len > 0 {
            first.insert(0, Piece { len, at: 0 });
        }
        PieceTable { chunks: vec![first], spare: Vec::new(), order: vec![0], totals: vec![len], near: (0, 0) }
    }

    /// Puts `len` deleted characters into the document at `pos`.
    pub fn insert_deleted(&mut self, pos: u32, len: u32) {
        let deleted = Piece { len, at: DELETED };
        loop {
            let (k, start) = self.chunk_at(pos);
            let chunk = &mut self.chunks[self.order[k] as usize];
            let (i, offset) = chunk.find(pos - start);
            let room = CHUNK - chunk.len();
            // A deleted piece they land in, or next to, takes them in.
            let joined = if offset > 0 {
                Some(i)
            } else if i > 0 && chunk.piece(i - 1).at == DELETED {
                Some(i - 1)
            } else {
                Some(i).filter(|&i| i < chunk.len())
            };
            match joined.map(|i| (i, chunk.piece(i))) {
                Some((i, piece)) if piece.at == DELETED => chunk.set(i, piece.with_len(piece.len + len)),
                // In the middle of text that survives, which is now in two parts.
                Some((i, piece)) if offset > 0 && room >= 2 => {
                    chunk.set(i, piece.with_len(offset));
                    chunk.insert(i + 1, deleted);
                    chunk.insert(i + 2, piece.after(offset));
                }
                _ if offset == 0 && room >= 1 => chunk.insert(i, deleted),
                _ => {
                    self.split(k);
                    continue;
                }
            }
            self.totals[k] += len;
            return;
        }
    }

    /// Takes the `len` characters at `pos` out of the document. For each stretch of them
    /// that survives, calls `survivor` with its position in the final text, how far into
    /// the removed characters it starts, and its length.
    pub fn remove(&mut self, pos: u32, len: u32, mut survivor: impl FnMut(u32, u32, u32)) {
        let mut done = 0;
        while done < len {
            let (mut k, mut start) = self.chunk_at(pos);
            if pos == start + self.totals[k] {
                start += self.totals[k];
                k += 1;
                self.near = (k, start);
            }
            let id = self.order[k];
            let chunk = &mut self.chunks[id as usize];
            let (i, offset) = chunk.find(pos - start);
            let piece = chunk.piece(i);
            let n = (piece.len - offset).min(len - done);
            // Surviving text with a piece taken out of its middle becomes two pieces: its
            // halves are no longer next to each other in the final text.
            let halved = offset > 0 && offset + n < piece.len && piece.at != DELETED;
            if halved && chunk.len() == CHUNK {
                self.split(k);
                continue;
            }
            if piece.at != DELETED {
                survivor(piece.at + offset, done, n);
            }
            // `seam`: the piece that now starts where the removed characters were.
            let seam = if offset == 0 && n == piece.len {
                chunk.remove(i);
                Some(i)
            } else if offset == 0 {
                chunk.set(i, piece.after(n));
                Some(i)
            } else if halved {
                chunk.set(i, piece.with_len(offset));
                chunk.insert(i + 1, piece.after(offset + n));
                None
            } else {
                chunk.set(i, piece.with_len(piece.len - n));
                (offset + n == piece.len).then_some(i + 1)
            };
            done += n;
            self.totals[k] -= n;

            // What was on either side of the removed characters may now be one piece.
            if let Some(i) = seam.filter(|&i| done == len && i > 0 && i < chunk.len()) {
                let (left, right) = (chunk.piece(i - 1), chunk.piece(i));
                if left.continues_into(right) {
                    chunk.set(i - 1, left.with_len(left.len + right.len));
                    chunk.remove(i);
                }
            }
            if chunk.len() == 0 && self.order.len() > 1 {
                self.order.remove(k);
                self.totals.remove(k);
                self.spare.push(id);
                self.near = if k < self.order.len() { (k, start) } else { (k - 1, start - self.totals[k - 1]) };
            }
        }
    }

    /// Finds the chunk that position `pos` is in, or at either end of: its place in `order`, and
    /// the position where it starts.
    fn chunk_at(&mut self, pos: u32) -> (usize, u32) {
        let (mut k, mut start) = self.near;
        while pos < start {
            k -= 1;
            start -= self.totals[k];
        }
        while pos > start + self.totals[k] {
            start += self.totals[k];
            k += 1;
        }
        self.near = (k, start);
        (k, start)
    }

    /// Moves the second half of a chunk's pieces into a new chunk after it.
    fn split(&mut self, k: usize) {
        let left = &mut self.chunks[self.order[k] as usize];
        let half = left.len / 2;
        let mut right = Chunk::EMPTY;
        right.len = left.len - half;
        right.pieces[..right.len].copy_from_slice(&left.pieces[half..left.len]);
        let moved = right.pieces[..right.len].iter().map(|piece| piece.len).sum::<u32>();
        left.len = half;
        left.last = (0, 0);

        let id = match self.spare.pop() {
            Some(id) => {
                self.chunks[id as usize] = right;
                id
            }
            None => {
                self.chunks.push(right);
                self.chunks.len() as u32 - 1
            }
        };
        self.order.insert(k + 1, id);
        self.totals.insert(k + 1, moved);
        self.totals[k] -= moved;
    }
}
