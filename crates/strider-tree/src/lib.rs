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
//! * Leaves and inner nodes are one struct, stored in two `Vec`s and linked by index, not
//!   by pointer.
//! * Select remembers the leaf it ended in. Events are merged in runs at one place, so the
//!   next select usually ends there too and skips the way down.

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

/// One child as a node holds it: which it is beside its widths, so that finding a child
/// and measuring up to it read the same memory.
#[derive(Clone, Copy)]
struct Slot {
    /// An entry's id in a leaf; the node one level down in an inner node.
    child: u32,
    /// The child's width in the prepare view: an entry's own, or the total under a node.
    prepare_width: u32,
    /// The child's width in the effect view.
    effect_width: u32,
}

impl Slot {
    const EMPTY: Slot = Slot { child: NONE, prepare_width: 0, effect_width: 0 };

    /// Creates the slot for `child`.
    fn new(child: u32, widths: Widths) -> Self {
        Slot { child, prepare_width: widths.prepare, effect_width: widths.effect }
    }

    /// Returns the child's widths.
    fn widths(&self) -> Widths {
        Widths { prepare: self.prepare_width, effect: self.effect_width }
    }

    /// Replaces the child's widths.
    fn set_widths(&mut self, widths: Widths) {
        (self.prepare_width, self.effect_width) = (widths.prepare, widths.effect);
    }
}

/// Up to `N` children in sequence order: entries in a leaf, nodes in an inner node. A node
/// knows its place under its parent, so an entry's character index is found by climbing
/// from its leaf.
struct Node<const N: usize> {
    /// How many of `slots` are in use.
    len: u32,
    parent: u32,
    /// This node's index among its parent's children.
    at: u32,
    slots: [Slot; N],
}

impl<const N: usize> Node<N> {
    /// Creates an empty node under `parent`.
    fn new(parent: u32) -> Self {
        Node { len: 0, parent, at: 0, slots: [Slot::EMPTY; N] }
    }

    /// Sums the widths of the children.
    fn total(&self) -> Widths {
        self.slots.iter().fold(Widths::default(), |total, slot| total.plus(slot.widths()))
    }

    /// Returns where a child starts in one view: the total width of the children before it.
    fn start(&self, view: View, child: usize) -> u32 {
        self.slots[..child].iter().map(|slot| slot.widths().of(view)).sum()
    }

    /// Finds the child holding character `idx` of one view, and how far into the child that
    /// character is. Nothing, if the children come to `idx` characters or fewer.
    fn find(&self, view: View, mut idx: u32) -> Option<(usize, u32)> {
        let mut child = 0;
        while idx >= self.slots[child].widths().of(view) {
            idx -= self.slots[child].widths().of(view);
            child += 1;
            if child >= self.len as usize {
                return None;
            }
        }
        Some((child, idx))
    }

    /// Returns which of the children `child` is.
    fn index_of(&self, child: u32) -> usize {
        if self.len <= 8 {
            // Nearly-empty nodes are the norm in small trees; a short scan beats the wide
            // one.
            return self.slots.iter().position(|slot| slot.child == child).expect("child is not in its node");
        }
        // Compare every slot with no early exit, so the loop is branch-free and
        // vectorises. Unused slots hold NONE and never match.
        let mut found = 0;
        for (i, slot) in self.slots.iter().enumerate() {
            found |= if slot.child == child { i } else { 0 };
        }
        debug_assert_eq!(self.slots[found].child, child);
        found
    }

    /// Adds a child as child number `at`, moving later ones along. There must be room.
    fn insert(&mut self, at: usize, child: u32, widths: Widths) {
        let len = self.len as usize;
        if at < len {
            self.slots.copy_within(at..len, at + 1);
        }
        self.slots[at] = Slot::new(child, widths);
        self.len += 1;
    }

    /// Splits child number `at` in two: a new child takes `widths` out of it, and sits just
    /// before it if `before`, otherwise just after. There must be room.
    fn split_child(&mut self, at: usize, child: u32, widths: Widths, before: bool) {
        let slot = &mut self.slots[at];
        slot.set_widths(slot.widths().minus(widths));
        self.insert(at + !before as usize, child, widths);
    }

    /// Moves the upper half of a full node's children to a new node, which it returns.
    fn split_off(&mut self) -> Node<N> {
        let half = N / 2;
        let mut upper = Node::new(self.parent);
        upper.len = (N - half) as u32;
        upper.slots[..N - half].copy_from_slice(&self.slots[half..]);
        self.slots[half..].fill(Slot::EMPTY);
        self.len = half as u32;
        upper
    }
}

/// `N` is how many entries a leaf holds and how many children an inner node has.
pub struct PrepareEffectTree<const N: usize = 32> {
    leaves: Vec<Node<N>>,
    nodes: Vec<Node<N>>,
    root: u32,
    /// Levels of inner nodes; 1-indexed.
    height: u32,
    /// The leaf each entry is in.
    home: Vec<u32>,
    /// The leaf the last select ended in, the view it was in, and the character index the
    /// leaf starts at in that view and in the effect view. Forgotten when widths change in
    /// any other leaf, since that may move where this one starts.
    last: Option<(View, u32, u32, u32)>,
}

impl<const N: usize> PrepareEffectTree<N> {
    /// Creates a tree with one entry.
    pub fn new(widths: Widths) -> Self {
        let mut tree = PrepareEffectTree { leaves: Vec::new(), nodes: Vec::new(), root: 0, height: 1, home: Vec::new(), last: None };
        tree.reset(widths);
        tree
    }

    /// Discards everything and starts again with one entry.
    pub fn reset(&mut self, widths: Widths) {
        self.leaves.clear();
        self.nodes.clear();
        self.home.clear();

        let mut leaf = Node::new(0);
        leaf.insert(0, 0, widths);
        self.leaves.push(leaf);
        let mut root = Node::new(NONE);
        root.insert(0, 0, widths);
        self.nodes.push(root);
        self.home.push(0);
        (self.root, self.height, self.last) = (0, 1, None);
    }

    /// Returns the number of entries.
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
            node = self.nodes[node as usize].slots[0].child;
        }
        Cursor { leaf: self.nodes[node as usize].slots[0].child, slot: 0 }
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
        let mut next = self.nodes[node as usize].slots[child + 1].child;
        for _ in 0..levels {
            next = self.nodes[next as usize].slots[0].child;
        }
        Some(Cursor { leaf: next, slot: 0 })
    }

    /// Returns the last leaf.
    fn last_leaf(&self) -> u32 {
        let mut node = self.root;
        for _ in 1..self.height {
            let nd = &self.nodes[node as usize];
            node = nd.slots[nd.len as usize - 1].child;
        }
        let nd = &self.nodes[node as usize];
        nd.slots[nd.len as usize - 1].child
    }

    /// Returns the id of the entry at `at`.
    pub fn id(&self, at: Cursor) -> Id {
        self.leaves[at.leaf as usize].slots[at.slot as usize].child
    }

    /// Returns the widths of the entry at `at`.
    pub fn widths(&self, at: Cursor) -> Widths {
        self.leaves[at.leaf as usize].slots[at.slot as usize].widths()
    }

    /// Finds where entry `id` is now.
    pub fn cursor(&self, id: Id) -> Cursor {
        let leaf = self.home[id as usize];
        Cursor { leaf, slot: self.leaves[leaf as usize].index_of(id) as u32 }
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
    pub fn char_idx_to_entry(&mut self, view: View, idx: u32) -> (Cursor, u32) {
        let (at, off, _) = self.locate(view, idx);
        (at, off)
    }

    /// Finds the entry holding character `idx` of one view, how far into the entry that
    /// character is, and the index the entry starts at in the effect view, which is summed on
    /// the way down.
    pub fn locate(&mut self, view: View, idx: u32) -> (Cursor, u32, u32) {
        if let Some((seen, leaf, start, effect)) = self.last {
            let found = if seen == view && idx >= start { self.leaves[leaf as usize].find(view, idx - start) } else { None };
            if let Some((slot, off)) = found {
                return (Cursor { leaf, slot: slot as u32 }, off, effect + self.leaves[leaf as usize].start(View::Effect, slot));
            }
        }
        let (mut node, mut rest, mut effect) = (self.root, idx, 0);
        for _ in 0..self.height {
            let nd = &self.nodes[node as usize];
            let child;
            (child, rest) = nd.find(view, rest).expect("index is past the end");
            effect += nd.start(View::Effect, child);
            node = nd.slots[child].child;
        }
        let leaf = &self.leaves[node as usize];
        let (slot, off) = leaf.find(view, rest).expect("index is past the end");
        let within = leaf.start(View::Effect, slot);
        self.last = Some((view, node, idx - rest, effect));
        (Cursor { leaf: node, slot: slot as u32 }, off, effect + within)
    }

    /// Notes that widths are changing in `leaf`.
    fn changing(&mut self, leaf: u32) {
        self.last = self.last.filter(|&(_, last, ..)| last == leaf);
    }

    /// Returns the character index an entry starts at in one view.
    pub fn entry_to_char_idx(&self, view: View, at: Cursor) -> u32 {
        let leaf = &self.leaves[at.leaf as usize];
        let mut idx = leaf.start(view, at.slot as usize);
        let (mut child, mut node) = (leaf.at as usize, leaf.parent);
        loop {
            let nd = &self.nodes[node as usize];
            idx += nd.start(view, child);
            if nd.parent == NONE {
                return idx;
            }
            (child, node) = (nd.at as usize, nd.parent);
        }
    }

    /// Changes an entry's widths.
    pub fn set_widths(&mut self, at: Cursor, widths: Widths) {
        let delta = self.replace(at, widths);
        self.widen(at.leaf, delta);
    }

    /// Adds `delta` to the widths recorded for `leaf` in each of its ancestors.
    fn widen(&mut self, leaf: u32, delta: Widths) {
        let leaf = &self.leaves[leaf as usize];
        let (mut child, mut node) = (leaf.at as usize, leaf.parent);
        loop {
            let nd = &mut self.nodes[node as usize];
            let slot = &mut nd.slots[child];
            slot.set_widths(slot.widths().plus(delta));
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
        self.changing(at.leaf);
        let slot = &mut self.leaves[at.leaf as usize].slots[at.slot as usize];
        let delta = widths.minus(slot.widths());
        slot.set_widths(widths);
        delta
    }

    /// Adds `delta` to the widths recorded for an entry's leaf in each of its ancestors,
    /// and returns the index the entry starts at in the effect view.
    fn climb(&mut self, at: Cursor, delta: Widths) -> u32 {
        let leaf = &self.leaves[at.leaf as usize];
        let mut idx = leaf.start(View::Effect, at.slot as usize);
        let (mut child, mut node) = (leaf.at as usize, leaf.parent);
        loop {
            let nd = &mut self.nodes[node as usize];
            idx += nd.start(View::Effect, child);
            let slot = &mut nd.slots[child];
            slot.set_widths(slot.widths().plus(delta));
            if nd.parent == NONE {
                return idx;
            }
            (child, node) = (nd.at as usize, nd.parent);
        }
    }

    /// Adds an entry just before `before`, or at the end for `None`. Returns its id and the
    /// index it starts at in the effect view.
    pub fn insert(&mut self, before: Option<Cursor>, widths: Widths) -> (Id, u32) {
        let (id, at) = self.place(before, widths);
        (id, self.climb(at, widths))
    }

    /// Adds an entry as [`insert`](Self::insert) does, for a caller that already knows where
    /// it starts. Returns its id.
    pub fn add(&mut self, before: Option<Cursor>, widths: Widths) -> Id {
        let (id, at) = self.place(before, widths);
        self.widen(at.leaf, widths);
        id
    }

    /// Puts a new entry in a leaf just before `before`, or at the end for `None`, leaving the
    /// leaf's ancestors to be told of its widths. Returns its id and where it is.
    fn place(&mut self, before: Option<Cursor>, widths: Widths) -> (Id, Cursor) {
        let before = before.unwrap_or_else(|| {
            let leaf = self.last_leaf();
            Cursor { leaf, slot: self.leaves[leaf as usize].len }
        });
        let at = self.make_room(before);
        self.changing(at.leaf);
        let id = self.new_entry(at.leaf);
        self.leaves[at.leaf as usize].insert(at.slot as usize, id, widths);
        (id, at)
    }

    /// Splits an entry in two: a new entry takes `widths` out of the entry at `at`, and
    /// sits just before it if `before`, otherwise just after. Returns the new entry's id
    /// and where the two now are, in sequence order.
    pub fn split_entry(&mut self, at: Cursor, widths: Widths, before: bool) -> (Id, [Cursor; 2]) {
        let at = self.make_room(at);
        self.changing(at.leaf);
        let id = self.new_entry(at.leaf);
        // The leaf's total is unchanged, so no ancestor needs updating.
        self.leaves[at.leaf as usize].split_child(at.slot as usize, id, widths, before);
        (id, [at, Cursor { leaf: at.leaf, slot: at.slot + 1 }])
    }

    /// Hands out the id for a new entry that is about to go into `leaf`.
    fn new_entry(&mut self, leaf: u32) -> Id {
        self.home.push(leaf);
        self.home.len() as Id - 1
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
        let upper = old.split_off();
        let parent = old.parent;
        for slot in &upper.slots[..N - half] {
            self.home[slot.child as usize] = new;
        }
        let moved = upper.total();
        self.leaves.push(upper);
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
            let child = self.nodes[node as usize].slots[i].child;
            self.adopt(level, child, node, i as u32 + 1);
        }
        self.nodes[node as usize].split_child(at, new, widths, false);
        self.adopt(level, new, node, at as u32 + 1);
    }

    /// Splits a full node, moving its upper half to a new sibling. Returns whichever of the
    /// two now holds `child`.
    fn split_node(&mut self, node: u32, child: u32, level: u32) -> u32 {
        let half = N / 2;
        let new = self.nodes.len() as u32;
        let old = &mut self.nodes[node as usize];
        let upper = old.split_off();
        let (parent, kept, moved) = (old.parent, old.total(), upper.total());
        let children = upper.slots.map(|slot| slot.child);
        self.nodes.push(upper);
        for (i, &c) in children[..N - half].iter().enumerate() {
            self.adopt(level, c, new, i as u32);
        }

        if parent == NONE {
            // Grow a new root above the two halves.
            let root = self.nodes.len() as u32;
            let mut top = Node::new(NONE);
            top.insert(0, node, kept);
            top.insert(1, new, moved);
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
        let node = if level == 1 { &mut self.leaves[child as usize] } else { &mut self.nodes[child as usize] };
        (node.parent, node.at) = (parent, at);
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
