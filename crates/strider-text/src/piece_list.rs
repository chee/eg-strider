use crate::piece::{Piece, DELETED};
use crate::piece_list_node::{PieceListNode, PIECES_PER_NODE};

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
