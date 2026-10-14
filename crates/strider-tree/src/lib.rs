use std::cmp::Ordering;

/// Id of an entry, handed out from 0 in order of creation.
pub type EntryId = u32;

/// Which version of the document a width or index counts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum View {
    /// The document as the merged event's author saw it.
    Prepare,
    /// The document as merged so far.
    Effect,
}

/// An entry's width in each view: its length where present, else 0.
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

    /// Adds widths, wrapping, so that a difference from [`minus`](Self::minus) adds back.
    fn plus(self, other: Widths) -> Widths {
        Widths { prepare: self.prepare.wrapping_add(other.prepare), effect: self.effect.wrapping_add(other.effect) }
    }

    /// Subtracts widths, wrapping.
    fn minus(self, other: Widths) -> Widths {
        Widths { prepare: self.prepare.wrapping_sub(other.prepare), effect: self.effect.wrapping_sub(other.effect) }
    }
}

const NONE: u32 = u32::MAX;

/// Where an entry sits now. Valid until the next [`insert`](PrepareEffectTree::insert) or
/// [`split_entry`](PrepareEffectTree::split_entry).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cursor {
    leaf: u32,
    slot: u32,
}

/// A child beside its widths, so that finding a child and measuring up to it read the same
/// memory.
#[derive(Clone, Copy)]
struct Slot {
    /// An entry's id in a leaf; a node one level down in an inner node.
    child: u32,
    /// The child's prepare width: an entry's own, or its subtree's total.
    prepare_width: u32,
    /// The child's effect width.
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

/// Up to `N` children in order: entries in a leaf, nodes in an inner node.
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

    /// Sums the children's widths.
    fn total(&self) -> Widths {
        self.slots.iter().fold(Widths::default(), |total, slot| total.plus(slot.widths()))
    }

    /// Returns where a child starts in one view.
    fn start(&self, view: View, child: usize) -> u32 {
        self.slots[..child].iter().map(|slot| slot.widths().of(view)).sum()
    }

    /// Finds the child holding character `index` of one view, and the offset into it. `None` if
    /// past the end.
    fn find(&self, view: View, mut index: u32) -> Option<(usize, u32)> {
        let mut child = 0;
        while index >= self.slots[child].widths().of(view) {
            index -= self.slots[child].widths().of(view);
            child += 1;
            if child >= self.len as usize {
                return None;
            }
        }
        Some((child, index))
    }

    /// Returns which child `child` is.
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

    /// Inserts a child at `at`. There must be room.
    fn insert(&mut self, at: usize, child: u32, widths: Widths) {
        let len = self.len as usize;
        if at < len {
            self.slots.copy_within(at..len, at + 1);
        }
        self.slots[at] = Slot::new(child, widths);
        self.len += 1;
    }

    /// Splits child `at`: a new child takes `widths` from it and goes just before it if
    /// `before`, else just after. There must be room.
    fn split_child(&mut self, at: usize, child: u32, widths: Widths, before: bool) {
        let slot = &mut self.slots[at];
        slot.set_widths(slot.widths().minus(widths));
        self.insert(at + !before as usize, child, widths);
    }

    /// Moves the upper half of a full node's children into a new node, which it returns.
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

/// Order-statistic B+ tree over the tracker's spans, measured in the prepare and effect
/// versions at once, with up to `N` children per node. Merging an event is a select in the
/// prepare version ([`char_index_to_entry`](Self::char_index_to_entry)), then a rank in the
/// effect version ([`entry_to_char_index`](Self::entry_to_char_index)).
///
/// Unlike a textbook B+ tree, inner nodes hold a width sum per version instead of keys; entries
/// are never removed, only set to width 0; nodes are linked to their parents by index, so rank
/// climbs from an entry's leaf; and select starts from the leaf it last ended in.
pub struct PrepareEffectTree<const N: usize = 32> {
    leaves: Vec<Node<N>>,
    nodes: Vec<Node<N>>,
    root: u32,
    /// Levels of inner nodes, from 1.
    height: u32,
    /// The leaf each entry is in.
    home: Vec<u32>,
    /// Where the last select ended: its leaf and view, and where that leaf starts in that view
    /// and in the effect view. Dropped when widths change in another leaf.
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

    /// Always false: a tree holds at least one entry.
    pub fn is_empty(&self) -> bool {
        false
    }

    /// Returns the first entry.
    pub fn first(&self) -> Cursor {
        let mut node_id = self.root;
        for _ in 1..self.height {
            node_id = self.nodes[node_id as usize].slots[0].child;
        }
        Cursor { leaf: self.nodes[node_id as usize].slots[0].child, slot: 0 }
    }

    /// Returns the entry after `at`, or `None` at the end.
    pub fn next(&self, at: Cursor) -> Option<Cursor> {
        if at.slot + 1 < self.leaves[at.leaf as usize].len {
            return Some(Cursor { leaf: at.leaf, slot: at.slot + 1 });
        }
        // Climb to the first ancestor with a child further right, then go down its left edge.
        let leaf = &self.leaves[at.leaf as usize];
        let (mut child, mut node_id, mut levels) = (leaf.at as usize, leaf.parent, 0);
        while child + 1 == self.nodes[node_id as usize].len as usize {
            let node = &self.nodes[node_id as usize];
            if node.parent == NONE {
                return None;
            }
            (child, node_id, levels) = (node.at as usize, node.parent, levels + 1);
        }
        let mut next = self.nodes[node_id as usize].slots[child + 1].child;
        for _ in 0..levels {
            next = self.nodes[next as usize].slots[0].child;
        }
        Some(Cursor { leaf: next, slot: 0 })
    }

    /// Returns the last leaf.
    fn last_leaf(&self) -> u32 {
        let mut node_id = self.root;
        for _ in 1..self.height {
            let node = &self.nodes[node_id as usize];
            node_id = node.slots[node.len as usize - 1].child;
        }
        let node = &self.nodes[node_id as usize];
        node.slots[node.len as usize - 1].child
    }

    /// Returns the id of the entry at `at`.
    pub fn id(&self, at: Cursor) -> EntryId {
        self.leaves[at.leaf as usize].slots[at.slot as usize].child
    }

    /// Returns the widths of the entry at `at`.
    pub fn widths(&self, at: Cursor) -> Widths {
        self.leaves[at.leaf as usize].slots[at.slot as usize].widths()
    }

    /// Finds where entry `id` is now.
    pub fn cursor(&self, id: EntryId) -> Cursor {
        let leaf = self.home[id as usize];
        Cursor { leaf, slot: self.leaves[leaf as usize].index_of(id) as u32 }
    }

    /// Compares two entries' places in the sequence.
    pub fn cmp(&self, a: Cursor, b: Cursor) -> Ordering {
        if a.leaf == b.leaf {
            return a.slot.cmp(&b.slot);
        }
        let levels = self.height as usize;
        self.path(a.leaf)[..levels].cmp(&self.path(b.leaf)[..levels])
    }

    /// Returns the child indexes from the root down to `leaf`.
    fn path(&self, leaf: u32) -> [u8; 16] {
        let mut path = [0; 16];
        let mut level = self.height as usize - 1;
        let leaf = &self.leaves[leaf as usize];
        path[level] = leaf.at as u8;
        let mut node_id = leaf.parent;
        while level > 0 {
            level -= 1;
            let node = &self.nodes[node_id as usize];
            path[level] = node.at as u8;
            node_id = node.parent;
        }
        path
    }

    /// Finds the entry holding character `index` of one view, and the offset into it.
    pub fn char_index_to_entry(&mut self, view: View, index: u32) -> (Cursor, u32) {
        let (at, offset, _) = self.locate(view, index);
        (at, offset)
    }

    /// As [`char_index_to_entry`](Self::char_index_to_entry), also returning where the entry
    /// starts in the effect view.
    pub fn locate(&mut self, view: View, index: u32) -> (Cursor, u32, u32) {
        if let Some((seen, leaf, start, effect)) = self.last {
            let found = if seen == view && index >= start { self.leaves[leaf as usize].find(view, index - start) } else { None };
            if let Some((slot, offset)) = found {
                return (Cursor { leaf, slot: slot as u32 }, offset, effect + self.leaves[leaf as usize].start(View::Effect, slot));
            }
        }
        let (mut node_id, mut rest, mut effect) = (self.root, index, 0);
        for _ in 0..self.height {
            let node = &self.nodes[node_id as usize];
            let child;
            (child, rest) = node.find(view, rest).expect("index is past the end");
            effect += node.start(View::Effect, child);
            node_id = node.slots[child].child;
        }
        let leaf = &self.leaves[node_id as usize];
        let (slot, offset) = leaf.find(view, rest).expect("index is past the end");
        let within = leaf.start(View::Effect, slot);
        self.last = Some((view, node_id, index - rest, effect));
        (Cursor { leaf: node_id, slot: slot as u32 }, offset, effect + within)
    }

    /// Notes that widths are changing in `leaf`.
    fn changing(&mut self, leaf: u32) {
        self.last = self.last.filter(|&(_, last, ..)| last == leaf);
    }

    /// Returns where an entry starts in one view.
    pub fn entry_to_char_index(&self, view: View, at: Cursor) -> u32 {
        let leaf = &self.leaves[at.leaf as usize];
        let mut index = leaf.start(view, at.slot as usize);
        let (mut child, mut node_id) = (leaf.at as usize, leaf.parent);
        loop {
            let node = &self.nodes[node_id as usize];
            index += node.start(view, child);
            if node.parent == NONE {
                return index;
            }
            (child, node_id) = (node.at as usize, node.parent);
        }
    }

    /// Changes an entry's widths.
    pub fn set_widths(&mut self, at: Cursor, widths: Widths) {
        let delta = self.replace(at, widths);
        self.widen(at.leaf, delta);
    }

    /// Adds `delta` to `leaf`'s widths in each of its ancestors.
    fn widen(&mut self, leaf: u32, delta: Widths) {
        let leaf = &self.leaves[leaf as usize];
        let (mut child, mut node_id) = (leaf.at as usize, leaf.parent);
        loop {
            let node = &mut self.nodes[node_id as usize];
            let slot = &mut node.slots[child];
            slot.set_widths(slot.widths().plus(delta));
            if node.parent == NONE {
                return;
            }
            (child, node_id) = (node.at as usize, node.parent);
        }
    }

    /// Changes an entry's widths and returns where it starts in the effect view.
    pub fn update(&mut self, at: Cursor, widths: Widths) -> u32 {
        let delta = self.replace(at, widths);
        self.climb(at, delta)
    }

    /// Sets an entry's widths and returns the change.
    fn replace(&mut self, at: Cursor, widths: Widths) -> Widths {
        self.changing(at.leaf);
        let slot = &mut self.leaves[at.leaf as usize].slots[at.slot as usize];
        let delta = widths.minus(slot.widths());
        slot.set_widths(widths);
        delta
    }

    /// Adds `delta` to an entry's leaf in each of its ancestors, and returns where the entry
    /// starts in the effect view.
    fn climb(&mut self, at: Cursor, delta: Widths) -> u32 {
        let leaf = &self.leaves[at.leaf as usize];
        let mut index = leaf.start(View::Effect, at.slot as usize);
        let (mut child, mut node_id) = (leaf.at as usize, leaf.parent);
        loop {
            let node = &mut self.nodes[node_id as usize];
            index += node.start(View::Effect, child);
            let slot = &mut node.slots[child];
            slot.set_widths(slot.widths().plus(delta));
            if node.parent == NONE {
                return index;
            }
            (child, node_id) = (node.at as usize, node.parent);
        }
    }

    /// Adds an entry before `before`, or at the end for `None`. Returns its id and where it
    /// starts in the effect view.
    pub fn insert(&mut self, before: Option<Cursor>, widths: Widths) -> (EntryId, u32) {
        let (id, at) = self.place(before, widths);
        (id, self.climb(at, widths))
    }

    /// Adds an entry as [`insert`](Self::insert) does, without working out where it starts.
    /// Returns its id.
    pub fn add(&mut self, before: Option<Cursor>, widths: Widths) -> EntryId {
        let (id, at) = self.place(before, widths);
        self.widen(at.leaf, widths);
        id
    }

    /// Puts a new entry in a leaf before `before`, or at the end for `None`, without updating
    /// ancestors. Returns its id and cursor.
    fn place(&mut self, before: Option<Cursor>, widths: Widths) -> (EntryId, Cursor) {
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

    /// Splits the entry at `at`: a new entry takes `widths` from it and goes just before it if
    /// `before`, else just after. Returns the new id and both cursors, in order.
    pub fn split_entry(&mut self, at: Cursor, widths: Widths, before: bool) -> (EntryId, [Cursor; 2]) {
        let at = self.make_room(at);
        self.changing(at.leaf);
        let id = self.new_entry(at.leaf);
        // The leaf's total is unchanged, so no ancestor needs updating.
        self.leaves[at.leaf as usize].split_child(at.slot as usize, id, widths, before);
        (id, [at, Cursor { leaf: at.leaf, slot: at.slot + 1 }])
    }

    /// Hands out the id of a new entry going into `leaf`.
    fn new_entry(&mut self, leaf: u32) -> EntryId {
        self.home.push(leaf);
        self.home.len() as EntryId - 1
    }

    /// Makes sure the leaf at `at` has a free slot. Returns where `at`'s slot is now.
    fn make_room(&mut self, at: Cursor) -> Cursor {
        if (self.leaves[at.leaf as usize].len as usize) < N {
            at
        } else {
            self.split_leaf(at)
        }
    }

    /// Splits a full leaf, moving its upper half to a new leaf after it. Returns where `at`'s
    /// slot is now.
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

    /// Adds `new` after `after` in node `node_id`, moving `widths` from `after` to it. `level`
    /// is 1 when the children are leaves.
    fn add_child(&mut self, mut node_id: u32, after: u32, new: u32, widths: Widths, level: u32) {
        if self.nodes[node_id as usize].len as usize == N {
            node_id = self.split_node(node_id, after, level);
        }
        let at = self.index_in_parent(level, after) as usize;
        let len = self.nodes[node_id as usize].len as usize;
        for i in at + 1..len {
            let child = self.nodes[node_id as usize].slots[i].child;
            self.adopt(level, child, node_id, i as u32 + 1);
        }
        self.nodes[node_id as usize].split_child(at, new, widths, false);
        self.adopt(level, new, node_id, at as u32 + 1);
    }

    /// Splits a full node, moving its upper half to a new sibling. Returns which of the two
    /// holds `child`.
    fn split_node(&mut self, node_id: u32, child: u32, level: u32) -> u32 {
        let half = N / 2;
        let new = self.nodes.len() as u32;
        let old = &mut self.nodes[node_id as usize];
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
            top.insert(0, node_id, kept);
            top.insert(1, new, moved);
            self.nodes.push(top);
            self.adopt(level + 1, node_id, root, 0);
            self.adopt(level + 1, new, root, 1);
            self.root = root;
            self.height += 1;
        } else {
            self.add_child(parent, node_id, new, moved, level + 1);
        }

        if children[..N - half].contains(&child) {
            new
        } else {
            node_id
        }
    }

    /// Records that `child` is child `at` of `parent`.
    fn adopt(&mut self, level: u32, child: u32, parent: u32, at: u32) {
        let node_id = if level == 1 { &mut self.leaves[child as usize] } else { &mut self.nodes[child as usize] };
        (node_id.parent, node_id.at) = (parent, at);
    }

    /// Returns `child`'s index among its parent's children.
    fn index_in_parent(&self, level: u32, child: u32) -> u32 {
        if level == 1 {
            self.leaves[child as usize].at
        } else {
            self.nodes[child as usize].at
        }
    }
}
