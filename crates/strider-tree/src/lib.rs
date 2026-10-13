//! An order-statistic B+ tree over the tracker's spans, counted in the prepare version and
//! the effect version at once.
//!
//! An entry is a span of characters, with a width in each version: its length where it is
//! present, 0 where it is not inserted yet or is deleted. `char_idx_to_entry` is select: it finds
//! the entry holding a character index of one view. `entry_to_char_idx` is rank: it gives
//! the index an entry starts at in one view. Merging an event
//! is a select in the prepare version followed by a rank in the effect version.
//!
//! Differences from a textbook B+ tree:
//!
//! * No keys. Entries are ordered by position, and inner nodes hold subtree sums instead
//!   of separator keys.
//! * Two sums per child, one for each version.
//! * Rank runs bottom-up. Nodes have parent pointers and the tree maps each entry's id to
//!   its leaf, so it climbs from the entry, adding up the siblings to its left.
//! * Insert-only. An entry is never deleted; its widths are set to 0. So there is no
//!   merging or rebalancing.
//! * Leaves and nodes are stored in two `Vec`s and linked by index, not by pointer.

use std::cmp::Ordering;

/// Names an entry. Ids are handed out in order of creation, starting from 0.
pub type Id = u32;

/// Which version of the document a width or a character index is counted in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum View {
    /// The document as the author of the event being merged saw it.
    Prepare,
    /// The document as merged so far.
    Effect,
}

/// An entry's width in each view: its length where it is present, 0 where it is not.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Widths {
    pub prepare: u32,
    pub effect: u32,
}

impl Widths {
    /// Returns the width in one view.
    fn of(self, view: View) -> u32 {
        match view {
            View::Prepare => self.prepare,
            View::Effect => self.effect,
        }
    }

    /// Adds widths, wrapping so that a difference taken with [`minus`](Self::minus) adds back.
    fn plus(self, other: Widths) -> Widths {
        Widths { prepare: self.prepare.wrapping_add(other.prepare), effect: self.effect.wrapping_add(other.effect) }
    }

    /// Subtracts widths, wrapping.
    fn minus(self, other: Widths) -> Widths {
        Widths { prepare: self.prepare.wrapping_sub(other.prepare), effect: self.effect.wrapping_sub(other.effect) }
    }
}

const NONE: u32 = u32::MAX;

/// Where an entry currently sits. A cursor stays valid until the next
/// [`insert`](PrepareEffectTree::insert) or [`split_entry`](PrepareEffectTree::split_entry);
/// get a fresh one with [`PrepareEffectTree::cursor`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cursor {
    leaf: u32,
    slot: u32,
}

/// One entry as a leaf holds it: its id beside its widths, so that finding an entry and
/// measuring up to it read the same memory.
#[derive(Clone, Copy)]
struct Slot {
    id: Id,
    prepare_width: u32,
    effect_width: u32,
}

impl Slot {
    const EMPTY: Slot = Slot { id: NONE, prepare_width: 0, effect_width: 0 };

    /// Creates the slot for entry `id`.
    fn new(id: Id, widths: Widths) -> Self {
        Slot { id, prepare_width: widths.prepare, effect_width: widths.effect }
    }

    /// Returns the entry's widths.
    fn widths(&self) -> Widths {
        Widths { prepare: self.prepare_width, effect: self.effect_width }
    }

    /// Replaces the entry's widths.
    fn set_widths(&mut self, widths: Widths) {
        (self.prepare_width, self.effect_width) = (widths.prepare, widths.effect);
    }
}

/// Up to `N` entries in sequence order. A leaf knows its place under its parent, so an
/// entry's character index is found by climbing from its leaf.
struct Leaf<const N: usize> {
    /// How many of `slots` are in use.
    len: u32,
    parent: u32,
    /// This leaf's index among its parent's children.
    at: u32,
    slots: [Slot; N],
}

/// An inner node: up to `N` children (leaves at the lowest level, nodes above), with the
/// total width under each. A lookup descends by these; an edit fixes them on the way up.
struct Node<const N: usize> {
    /// How many of `children` are in use.
    len: u32,
    parent: u32,
    /// This node's index among its parent's children.
    at: u32,
    children: [u32; N],
    /// The total width under each child in the prepare view.
    prepare_width: [u32; N],
    /// The total width under each child in the effect view.
    effect_width: [u32; N],
}

impl<const N: usize> Node<N> {
    /// Creates an empty node under `parent`.
    fn new(parent: u32) -> Self {
        Node { len: 0, parent, at: 0, children: [NONE; N], prepare_width: [0; N], effect_width: [0; N] }
    }

    /// Returns the width under each child in one view.
    fn widths(&self, view: View) -> &[u32; N] {
        match view {
            View::Prepare => &self.prepare_width,
            View::Effect => &self.effect_width,
        }
    }

    /// Returns the widths under one child.
    fn width(&self, child: usize) -> Widths {
        Widths { prepare: self.prepare_width[child], effect: self.effect_width[child] }
    }

    /// Replaces the widths under one child.
    fn set_width(&mut self, child: usize, widths: Widths) {
        (self.prepare_width[child], self.effect_width[child]) = (widths.prepare, widths.effect);
    }

    /// Sums the widths under this node.
    fn total(&self) -> Widths {
        let len = self.len as usize;
        Widths { prepare: self.prepare_width[..len].iter().sum(), effect: self.effect_width[..len].iter().sum() }
    }
}

/// `N` is how many entries a leaf holds and how many children a node has.
pub struct PrepareEffectTree<const N: usize = 32> {
    leaves: Vec<Leaf<N>>,
    nodes: Vec<Node<N>>,
    root: u32,
    /// Levels of internal nodes; 1-indexed.
    height: u32,
    /// The leaf each entry is in.
    home: Vec<u32>,
}

impl<const N: usize> PrepareEffectTree<N> {
    /// Initialize a new strider-tree
    pub fn new(widths: Widths) -> Self {
        let mut tree = PrepareEffectTree { leaves: Vec::new(), nodes: Vec::new(), root: 0, height: 1, home: Vec::new() };
        tree.reset(widths);
        tree
    }

    /// Discards everything and starts again with one entry
    pub fn reset(&mut self, widths: Widths) {
        self.leaves.clear();
        self.nodes.clear();
        self.home.clear();

        let mut leaf = Leaf { len: 1, parent: 0, at: 0, slots: [Slot::EMPTY; N] };
        leaf.slots[0] = Slot::new(0, widths);
        self.leaves.push(leaf);
        let mut root = Node::new(NONE);
        root.len = 1;
        root.children[0] = 0;
        root.set_width(0, widths);
        self.nodes.push(root);
        self.home.push(0);
        (self.root, self.height) = (0, 1);
    }

    /// Number of entries in the tree
    pub fn len(&self) -> usize {
        self.home.len()
    }

    /// Returns false: a tree always holds at least one entry.
    pub fn is_empty(&self) -> bool {
        false
    }

    /// Returns a cursor to the first entry.
    pub fn first(&self) -> Cursor {
        let mut node = self.root;
        for _ in 1..self.height {
            node = self.nodes[node as usize].children[0];
        }
        Cursor { leaf: self.nodes[node as usize].children[0], slot: 0 }
    }

    /// Returns a cursor to the entry after `at`, or `None` at the end.
    pub fn next(&self, at: Cursor) -> Option<Cursor> {
        if at.slot + 1 < self.leaves[at.leaf as usize].len {
            return Some(Cursor { leaf: at.leaf, slot: at.slot + 1 });
        }
        // Climb to the first ancestor with a child further right, then go down its left edge.
        let leaf = &self.leaves[at.leaf as usize];
        let (mut child, mut node, mut levels) = (leaf.at as usize, leaf.parent, 0);
        while child + 1 == self.nodes[node as usize].len as usize {
            let nd = &self.nodes[node as usize];
            if nd.parent == NONE {
                return None;
            }
            (child, node, levels) = (nd.at as usize, nd.parent, levels + 1);
        }
        let mut next = self.nodes[node as usize].children[child + 1];
        for _ in 0..levels {
            next = self.nodes[next as usize].children[0];
        }
        Some(Cursor { leaf: next, slot: 0 })
    }

    /// Returns the last leaf.
    fn last_leaf(&self) -> u32 {
        let mut node = self.root;
        for _ in 1..self.height {
            let nd = &self.nodes[node as usize];
            node = nd.children[nd.len as usize - 1];
        }
        let nd = &self.nodes[node as usize];
        nd.children[nd.len as usize - 1]
    }

    /// Returns the id of the entry at `at`.
    pub fn id(&self, at: Cursor) -> Id {
        self.leaves[at.leaf as usize].slots[at.slot as usize].id
    }

    /// Returns the widths of the entry at `at`.
    pub fn widths(&self, at: Cursor) -> Widths {
        self.leaves[at.leaf as usize].slots[at.slot as usize].widths()
    }

    /// Finds where entry `id` is now.
    pub fn cursor(&self, id: Id) -> Cursor {
        let leaf = self.home[id as usize];
        let slots = &self.leaves[leaf as usize].slots;
        let slot = if self.leaves[leaf as usize].len <= 8 {
            // Nearly-empty leaves are the norm in small trees; a short scan beats the wide
            // one.
            slots.iter().position(|s| s.id == id).expect("entry is not in its leaf")
        } else {
            // Compare every slot with no early exit, so the loop is branch-free and
            // vectorises. Unused slots hold NONE and never match.
            let mut found = 0;
            for (i, s) in slots.iter().enumerate() {
                found |= if s.id == id { i } else { 0 };
            }
            debug_assert_eq!(slots[found].id, id);
            found
        };
        Cursor { leaf, slot: slot as u32 }
    }

    /// Compares two entries by their place in the sequence.
    pub fn cmp(&self, a: Cursor, b: Cursor) -> Ordering {
        if a.leaf == b.leaf {
            return a.slot.cmp(&b.slot);
        }
        let levels = self.height as usize;
        self.path(a.leaf)[..levels].cmp(&self.path(b.leaf)[..levels])
    }

    /// Collects the child indexes from the root down to `leaf`.
    fn path(&self, leaf: u32) -> [u8; 16] {
        let mut path = [0; 16];
        let mut level = self.height as usize - 1;
        let leaf = &self.leaves[leaf as usize];
        path[level] = leaf.at as u8;
        let mut node = leaf.parent;
        while level > 0 {
            level -= 1;
            let nd = &self.nodes[node as usize];
            path[level] = nd.at as u8;
            node = nd.parent;
        }
        path
    }

    /// Finds the entry holding character `idx` of one view, and how far into the entry that
    /// character is.
    pub fn char_idx_to_entry(&self, view: View, mut idx: u32) -> (Cursor, u32) {
        let mut node = self.root;
        for _ in 0..self.height {
            let nd = &self.nodes[node as usize];
            let widths = nd.widths(view);
            let mut child = 0;
            while idx >= widths[child] {
                idx -= widths[child];
                child += 1;
                assert!(child < nd.len as usize, "index is past the end");
            }
            node = nd.children[child];
        }
        let leaf = &self.leaves[node as usize];
        let mut slot = 0;
        while idx >= leaf.slots[slot].widths().of(view) {
            idx -= leaf.slots[slot].widths().of(view);
            slot += 1;
        }
        (Cursor { leaf: node, slot: slot as u32 }, idx)
    }

    /// Returns the character index an entry starts at in one view.
    pub fn entry_to_char_idx(&self, view: View, at: Cursor) -> u32 {
        let leaf = &self.leaves[at.leaf as usize];
        let mut idx: u32 = leaf.slots[..at.slot as usize].iter().map(|slot| slot.widths().of(view)).sum();
        let (mut child, mut node) = (leaf.at as usize, leaf.parent);
        loop {
            let nd = &self.nodes[node as usize];
            idx += nd.widths(view)[..child].iter().sum::<u32>();
            if nd.parent == NONE {
                return idx;
            }
            (child, node) = (nd.at as usize, nd.parent);
        }
    }

    /// Changes an entry's widths.
    pub fn set_widths(&mut self, at: Cursor, widths: Widths) {
        let delta = self.replace(at, widths);
        let leaf = &self.leaves[at.leaf as usize];
        let (mut child, mut node) = (leaf.at as usize, leaf.parent);
        loop {
            let nd = &mut self.nodes[node as usize];
            nd.set_width(child, nd.width(child).plus(delta));
            if nd.parent == NONE {
                return;
            }
            (child, node) = (nd.at as usize, nd.parent);
        }
    }

    /// Changes an entry's widths and returns the index it starts at in the effect view, found
    /// on the same pass up the tree.
    pub fn update(&mut self, at: Cursor, widths: Widths) -> u32 {
        let delta = self.replace(at, widths);
        self.climb(at, delta)
    }

    /// Stores new widths in an entry's slot and returns the change.
    fn replace(&mut self, at: Cursor, widths: Widths) -> Widths {
        let slot = &mut self.leaves[at.leaf as usize].slots[at.slot as usize];
        let delta = widths.minus(slot.widths());
        slot.set_widths(widths);
        delta
    }

    /// Adds `delta` to the widths recorded for an entry's leaf in each of its ancestors,
    /// and returns the index the entry starts at in the effect view.
    fn climb(&mut self, at: Cursor, delta: Widths) -> u32 {
        let leaf = &self.leaves[at.leaf as usize];
        let mut idx: u32 = leaf.slots[..at.slot as usize].iter().map(|slot| slot.effect_width).sum();
        let (mut child, mut node) = (leaf.at as usize, leaf.parent);
        loop {
            let nd = &mut self.nodes[node as usize];
            idx += nd.effect_width[..child].iter().sum::<u32>();
            nd.set_width(child, nd.width(child).plus(delta));
            if nd.parent == NONE {
                return idx;
            }
            (child, node) = (nd.at as usize, nd.parent);
        }
    }

    /// Adds an entry just before `before`, or at the end for `None`. Returns its id and the
    /// index it starts at in the effect view.
    pub fn insert(&mut self, before: Option<Cursor>, widths: Widths) -> (Id, u32) {
        let before = before.unwrap_or_else(|| {
            let leaf = self.last_leaf();
            Cursor { leaf, slot: self.leaves[leaf as usize].len }
        });
        let at = self.make_room(before);
        let id = self.write_slot(at, widths);
        (id, self.climb(at, widths))
    }

    /// Splits an entry in two: a new entry takes `widths` out of the entry at `at`, and
    /// sits just before it if `before`, otherwise just after.
    pub fn split_entry(&mut self, at: Cursor, widths: Widths, before: bool) -> Id {
        let at = self.make_room(at);
        let slot = &mut self.leaves[at.leaf as usize].slots[at.slot as usize];
        slot.set_widths(slot.widths().minus(widths));
        // The leaf's total is unchanged, so no ancestor needs updating.
        self.write_slot(Cursor { leaf: at.leaf, slot: at.slot + !before as u32 }, widths)
    }

    /// Writes a new entry into the slot at `at`, shifting later ones along. The leaf must
    /// have room.
    fn write_slot(&mut self, at: Cursor, widths: Widths) -> Id {
        let id = self.home.len() as Id;
        self.home.push(at.leaf);
        let leaf = &mut self.leaves[at.leaf as usize];
        let (slot, len) = (at.slot as usize, leaf.len as usize);
        if slot < len {
            leaf.slots.copy_within(slot..len, slot + 1);
        }
        leaf.slots[slot] = Slot::new(id, widths);
        leaf.len += 1;
        id
    }

    /// Ensures the leaf at `at` has a free slot. Returns where the slot that was at `at`
    /// is now.
    fn make_room(&mut self, at: Cursor) -> Cursor {
        if (self.leaves[at.leaf as usize].len as usize) < N {
            at
        } else {
            self.split_leaf(at)
        }
    }

    /// Splits a full leaf, moving its upper half to a new leaf after it. Returns where the
    /// slot that was at `at` is now.
    fn split_leaf(&mut self, at: Cursor) -> Cursor {
        let half = N / 2;
        let new = self.leaves.len() as u32;
        let old = &mut self.leaves[at.leaf as usize];
        let mut right = Leaf { len: (N - half) as u32, parent: old.parent, at: 0, slots: [Slot::EMPTY; N] };
        right.slots[..N - half].copy_from_slice(&old.slots[half..]);
        old.slots[half..].fill(Slot::EMPTY);
        old.len = half as u32;
        let parent = old.parent;

        let mut moved = Widths::default();
        for slot in &right.slots[..N - half] {
            self.home[slot.id as usize] = new;
            moved = moved.plus(slot.widths());
        }
        self.leaves.push(right);
        self.add_child(parent, at.leaf, new, moved, 1);

        if at.slot as usize >= half {
            Cursor { leaf: new, slot: at.slot - half as u32 }
        } else {
            at
        }
    }

    /// Adds `new` as the child after `after` in `node`, taking `widths` of what was
    /// recorded for `after` with it. `level` is 1 when the children are leaves.
    fn add_child(&mut self, mut node: u32, after: u32, new: u32, widths: Widths, level: u32) {
        if self.nodes[node as usize].len as usize == N {
            node = self.split_node(node, after, level);
        }
        let at = self.index_in_parent(level, after) as usize;
        let len = self.nodes[node as usize].len as usize;
        for i in at + 1..len {
            let child = self.nodes[node as usize].children[i];
            self.adopt(level, child, node, i as u32 + 1);
        }
        let nd = &mut self.nodes[node as usize];
        nd.children.copy_within(at + 1..len, at + 2);
        nd.children[at + 1] = new;
        nd.prepare_width.copy_within(at + 1..len, at + 2);
        nd.effect_width.copy_within(at + 1..len, at + 2);
        nd.set_width(at + 1, widths);
        nd.set_width(at, nd.width(at).minus(widths));
        nd.len += 1;
        self.adopt(level, new, node, at as u32 + 1);
    }

    /// Splits a full node, moving its upper half to a new sibling. Returns whichever of the
    /// two now holds `child`.
    fn split_node(&mut self, node: u32, child: u32, level: u32) -> u32 {
        let half = N / 2;
        let new = self.nodes.len() as u32;
        let old = &mut self.nodes[node as usize];
        let mut right = Node::new(old.parent);
        right.len = (N - half) as u32;
        right.children[..N - half].copy_from_slice(&old.children[half..]);
        old.children[half..].fill(NONE);
        right.prepare_width[..N - half].copy_from_slice(&old.prepare_width[half..]);
        right.effect_width[..N - half].copy_from_slice(&old.effect_width[half..]);
        old.len = half as u32;
        let parent = old.parent;
        let moved = right.total();
        let children = right.children;
        self.nodes.push(right);
        for (i, &c) in children[..N - half].iter().enumerate() {
            self.adopt(level, c, new, i as u32);
        }

        if parent == NONE {
            // Grow a new root above the two halves.
            let root = self.nodes.len() as u32;
            let old = &self.nodes[node as usize];
            let mut top = Node::new(NONE);
            top.len = 2;
            top.children[..2].copy_from_slice(&[node, new]);
            top.set_width(0, old.total());
            top.set_width(1, moved);
            self.nodes.push(top);
            self.adopt(level + 1, node, root, 0);
            self.adopt(level + 1, new, root, 1);
            self.root = root;
            self.height += 1;
        } else {
            self.add_child(parent, node, new, moved, level + 1);
        }

        if children[..N - half].contains(&child) {
            new
        } else {
            node
        }
    }

    /// Records that `child` is child number `at` of `parent`.
    fn adopt(&mut self, level: u32, child: u32, parent: u32, at: u32) {
        if level == 1 {
            let leaf = &mut self.leaves[child as usize];
            (leaf.parent, leaf.at) = (parent, at);
        } else {
            let node = &mut self.nodes[child as usize];
            (node.parent, node.at) = (parent, at);
        }
    }

    /// Returns which child of its parent `child` is.
    fn index_in_parent(&self, level: u32, child: u32) -> u32 {
        if level == 1 {
            self.leaves[child as usize].at
        } else {
            self.nodes[child as usize].at
        }
    }
}
