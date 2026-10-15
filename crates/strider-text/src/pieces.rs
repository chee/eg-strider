/// In place of a final position: these characters do not survive.
const DELETED: u32 = u32::MAX;

/// Pieces per node. Tiny in debug builds, so that tests split and empty nodes often.
const PIECES_PER_NODE: usize = if cfg!(debug_assertions) { 4 } else { 32 };

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

    /// Tells whether `next`, which follows this piece, shares its fate: both deleted, or both
    /// surviving side by side.
    fn continues_into(self, next: Piece) -> bool {
        if self.at == DELETED || next.at == DELETED {
            self.at == next.at
        } else {
            self.at + self.len == next.at
        }
    }
}

/// Up to [`PIECES_PER_NODE`] pieces, in document order.
#[derive(Clone, Copy)]
struct PieceListNode {
    len: usize,
    /// The finger: the piece found last, and the characters before it in the node. The next
    /// lookup usually lands near it, on either side.
    finger: (usize, u32),
    pieces: [Piece; PIECES_PER_NODE],
}

impl PieceListNode {
    const EMPTY: PieceListNode = PieceListNode { len: 0, finger: (0, 0), pieces: [Piece { len: 0, at: 0 }; PIECES_PER_NODE] };

    /// Returns the number of pieces.
    fn len(&self) -> usize {
        self.len
    }

    /// Returns piece `i`.
    fn piece(&self, i: usize) -> Piece {
        self.pieces[i]
    }

    /// Notes that piece `i` is changing, which moves any remembered piece after it.
    fn touch(&mut self, i: usize) {
        if i < self.finger.0 {
            self.finger = (0, 0);
        }
    }

    /// Replaces piece `i`.
    fn set(&mut self, i: usize, piece: Piece) {
        self.touch(i);
        self.pieces[i] = piece;
    }

    /// Finds the piece holding character `offset` of the node, and the offset into it. One
    /// past the last piece at the node's end.
    fn find(&mut self, offset: u32) -> (usize, u32) {
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

/// The pieces of a [`PieceTable`](crate::PieceTable), in order: stretches of the document that
/// either sit at a known place in the final text or are deleted later. They are kept in an
/// unrolled linked list of nodes, and the node and piece used last are tried first.
pub(crate) struct PieceList {
    nodes: Vec<PieceListNode>,
    /// Nodes no longer in use.
    spare: Vec<u32>,
    /// The nodes in document order, and how many characters each holds.
    order: Vec<u32>,
    totals: Vec<u32>,
    /// The finger: the node used last, its place in `order`, and its start.
    finger: (usize, u32),
}

impl PieceList {
    /// Creates a document of `len` characters that all survive: the final text.
    pub fn new(len: u32) -> Self {
        let mut first = PieceListNode::EMPTY;
        if len > 0 {
            first.insert(0, Piece { len, at: 0 });
        }
        PieceList { nodes: vec![first], spare: Vec::new(), order: vec![0], totals: vec![len], finger: (0, 0) }
    }

    /// Puts `len` deleted characters into the document at `pos`.
    pub fn insert_deleted(&mut self, pos: u32, len: u32) {
        let deleted = Piece { len, at: DELETED };
        loop {
            let (place, start) = self.node_at(pos);
            let node = &mut self.nodes[self.order[place] as usize];
            let (i, offset) = node.find(pos - start);
            let room = PIECES_PER_NODE - node.len();
            // A deleted piece they land in, or next to, takes them in.
            let joined = if offset > 0 {
                Some(i)
            } else if i > 0 && node.piece(i - 1).at == DELETED {
                Some(i - 1)
            } else {
                Some(i).filter(|&i| i < node.len())
            };
            match joined.map(|i| (i, node.piece(i))) {
                Some((i, piece)) if piece.at == DELETED => node.set(i, piece.with_len(piece.len + len)),
                // In the middle of text that survives, which is now in two parts.
                Some((i, piece)) if offset > 0 && room >= 2 => {
                    node.set(i, piece.with_len(offset));
                    node.insert(i + 1, deleted);
                    node.insert(i + 2, piece.after(offset));
                }
                _ if offset == 0 && room >= 1 => node.insert(i, deleted),
                _ => {
                    self.split(place);
                    continue;
                }
            }
            self.totals[place] += len;
            return;
        }
    }

    /// Takes the `len` characters at `pos` out of the document, calling `survivor` for each
    /// surviving stretch with its final position, its offset into the removed characters, and
    /// its length.
    pub fn remove(&mut self, pos: u32, len: u32, mut survivor: impl FnMut(u32, u32, u32)) {
        let mut done = 0;
        while done < len {
            let (mut place, mut start) = self.node_at(pos);
            if pos == start + self.totals[place] {
                start += self.totals[place];
                place += 1;
                self.finger = (place, start);
            }
            let id = self.order[place];
            let node = &mut self.nodes[id as usize];
            let (i, offset) = node.find(pos - start);
            let piece = node.piece(i);
            let n = (piece.len - offset).min(len - done);
            // Surviving text with a piece taken out of its middle becomes two pieces: its
            // halves are no longer next to each other in the final text.
            let halved = offset > 0 && offset + n < piece.len && piece.at != DELETED;
            if halved && node.len() == PIECES_PER_NODE {
                self.split(place);
                continue;
            }
            if piece.at != DELETED {
                survivor(piece.at + offset, done, n);
            }
            // `seam`: the piece that now starts where the removed characters were.
            let seam = if offset == 0 && n == piece.len {
                node.remove(i);
                Some(i)
            } else if offset == 0 {
                node.set(i, piece.after(n));
                Some(i)
            } else if halved {
                node.set(i, piece.with_len(offset));
                node.insert(i + 1, piece.after(offset + n));
                None
            } else {
                node.set(i, piece.with_len(piece.len - n));
                (offset + n == piece.len).then_some(i + 1)
            };
            done += n;
            self.totals[place] -= n;

            // What was on either side of the removed characters may now be one piece.
            if let Some(i) = seam.filter(|&i| done == len && i > 0 && i < node.len()) {
                let (left, right) = (node.piece(i - 1), node.piece(i));
                if left.continues_into(right) {
                    node.set(i - 1, left.with_len(left.len + right.len));
                    node.remove(i);
                }
            }
            if node.len() == 0 && self.order.len() > 1 {
                self.order.remove(place);
                self.totals.remove(place);
                self.spare.push(id);
                self.finger = if place < self.order.len() { (place, start) } else { (place - 1, start - self.totals[place - 1]) };
            }
        }
    }

    /// Iterates over the pieces in order: each one's length, and its final position if it
    /// survives.
    pub fn iter(&self) -> impl Iterator<Item = (u32, Option<u32>)> + '_ {
        let pieces = self.order.iter().flat_map(|&id| {
            let node = &self.nodes[id as usize];
            &node.pieces[..node.len]
        });
        pieces.map(|piece| (piece.len, Some(piece.at).filter(|&at| at != DELETED)))
    }

    /// Finds the node that `pos` is in or at either end of: its place in `order`, and its
    /// start.
    fn node_at(&mut self, pos: u32) -> (usize, u32) {
        let (mut place, mut start) = self.finger;
        while pos < start {
            place -= 1;
            start -= self.totals[place];
        }
        while pos > start + self.totals[place] {
            start += self.totals[place];
            place += 1;
        }
        self.finger = (place, start);
        (place, start)
    }

    /// Moves the second half of a node's pieces into a new node after it.
    fn split(&mut self, place: usize) {
        let left = &mut self.nodes[self.order[place] as usize];
        let half = left.len / 2;
        let mut right = PieceListNode::EMPTY;
        right.len = left.len - half;
        right.pieces[..right.len].copy_from_slice(&left.pieces[half..left.len]);
        let moved = right.pieces[..right.len].iter().map(|piece| piece.len).sum::<u32>();
        left.len = half;
        left.finger = (0, 0);

        let id = match self.spare.pop() {
            Some(id) => {
                self.nodes[id as usize] = right;
                id
            }
            None => {
                self.nodes.push(right);
                self.nodes.len() as u32 - 1
            }
        };
        self.order.insert(place + 1, id);
        self.totals.insert(place + 1, moved);
        self.totals[place] -= moved;
    }
}
