use crate::{Lengths, View};

pub(crate) const NO_NODE: u32 = u32::MAX;

/// A child beside its lengths, so that finding a child and measuring up to it read the same
/// memory.
#[derive(Clone, Copy)]
pub(crate) struct Slot {
    /// An entry's id in a leaf; a node one level down in an inner node.
    pub(crate) child: u32,
    /// The child's prepare length: an entry's own, or its subtree's total.
    pub(crate) prepare_len: u32,
    /// The child's effect length.
    pub(crate) effect_len: u32,
}

impl Slot {
    pub(crate) const EMPTY: Slot = Slot { child: NO_NODE, prepare_len: 0, effect_len: 0 };

    /// Creates the slot for `child`.
    pub(crate) fn new(child: u32, lengths: Lengths) -> Self {
        Slot { child, prepare_len: lengths.prepare, effect_len: lengths.effect }
    }

    /// Returns the child's lengths.
    pub(crate) fn lengths(&self) -> Lengths {
        Lengths { prepare: self.prepare_len, effect: self.effect_len }
    }

    /// Replaces the child's lengths.
    pub(crate) fn set_lengths(&mut self, lengths: Lengths) {
        (self.prepare_len, self.effect_len) = (lengths.prepare, lengths.effect);
    }
}

/// Up to `N` children in order: entries in a leaf, nodes in an inner node.
pub(crate) struct Node<const N: usize> {
    /// How many of `slots` are in use.
    pub(crate) len: u32,
    pub(crate) parent: u32,
    /// This node's index among its parent's children.
    pub(crate) at: u32,
    pub(crate) slots: [Slot; N],
}

impl<const N: usize> Node<N> {
    /// Creates an empty node under `parent`.
    pub(crate) fn new(parent: u32) -> Self {
        Node { len: 0, parent, at: 0, slots: [Slot::EMPTY; N] }
    }

    /// Sums the children's lengths.
    pub(crate) fn total(&self) -> Lengths {
        self.slots.iter().fold(Lengths::default(), |total, slot| total.plus(slot.lengths()))
    }

    /// Returns where a child starts in one view.
    pub(crate) fn start(&self, view: View, child: usize) -> u32 {
        self.slots[..child].iter().map(|slot| slot.lengths().of(view)).sum()
    }

    /// Finds the child holding character `index` of one view, and the offset into it. `None` if
    /// past the end.
    pub(crate) fn find(&self, view: View, mut index: u32) -> Option<(usize, u32)> {
        let mut child = 0;
        while index >= self.slots[child].lengths().of(view) {
            index -= self.slots[child].lengths().of(view);
            child += 1;
            if child >= self.len as usize {
                return None;
            }
        }
        Some((child, index))
    }

    /// Returns which child `child` is.
    pub(crate) fn index_of(&self, child: u32) -> usize {
        if self.len <= 8 {
            // Nearly-empty nodes are the norm in small trees; a short scan beats the wide
            // one.
            return self.slots.iter().position(|slot| slot.child == child).expect("child is not in its node");
        }
        // Compare every slot with no early exit, so the loop is branch-free and
        // vectorises. Unused slots hold NO_NODE and never match.
        let mut found = 0;
        for (i, slot) in self.slots.iter().enumerate() {
            found |= if slot.child == child { i } else { 0 };
        }
        debug_assert_eq!(self.slots[found].child, child);
        found
    }

    /// Inserts a child at `at`. There must be room.
    pub(crate) fn insert(&mut self, at: usize, child: u32, lengths: Lengths) {
        let len = self.len as usize;
        if at < len {
            self.slots.copy_within(at..len, at + 1);
        }
        self.slots[at] = Slot::new(child, lengths);
        self.len += 1;
    }

    /// Splits child `at`: a new child takes `lengths` from it and goes just before it if
    /// `before`, else just after. There must be room.
    pub(crate) fn split_child(&mut self, at: usize, child: u32, lengths: Lengths, before: bool) {
        let slot = &mut self.slots[at];
        slot.set_lengths(slot.lengths().minus(lengths));
        self.insert(at + !before as usize, child, lengths);
    }

    /// Moves the upper half of a full node's children into a new node, which it returns.
    pub(crate) fn split_off(&mut self) -> Node<N> {
        let half = N / 2;
        let mut upper = Node::new(self.parent);
        upper.len = (N - half) as u32;
        upper.slots[..N - half].copy_from_slice(&self.slots[half..]);
        self.slots[half..].fill(Slot::EMPTY);
        self.len = half as u32;
        upper
    }
}
