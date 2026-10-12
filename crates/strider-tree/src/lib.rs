//! A sequence of entries that can be measured in two *views* at once.

use std::cmp::Ordering;

/// Names an entry. Ids are handed out in order of creation, starting from 0.
pub type Id = u32;

const NONE: u32 = u32::MAX;

/// Where an entry currently sits. A cursor stays valid until the next
/// [`insert`](Tree::insert) or [`split_off`](Tree::split_off); get a fresh one with
/// [`Tree::cursor`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cursor {
    leaf: u32,
    slot: u32,
}

#[derive(Clone, Copy)]
struct Slot {
    id: Id,
    widths: [u32; 2],
}

const EMPTY: Slot = Slot { id: NONE, widths: [0; 2] };

struct Leaf<const N: usize> {
    len: u32,
    parent: u32,
    /// This leaf's index among its parent's children.
    at: u32,
    next: u32,
    slots: [Slot; N],
}

struct Node<const N: usize> {
    len: u32,
    parent: u32,
    /// This node's index among its parent's children.
    at: u32,
    children: [u32; N],
    /// Per view, the total width under each child.
    widths: [[u32; N]; 2],
}

impl<const N: usize> Node<N> {
    fn new(parent: u32) -> Self {
        Node { len: 0, parent, at: 0, children: [NONE; N], widths: [[0; N]; 2] }
    }

    fn total(&self, view: usize) -> u32 {
        self.widths[view][..self.len as usize].iter().sum()
    }
}

/// `N` is how many entries a leaf holds and how many children a node has.
pub struct Tree<const N: usize = 32> {
    leaves: Vec<Leaf<N>>,
    nodes: Vec<Node<N>>,
    root: u32,
    /// Levels of internal nodes; the root's children are leaves when this is 1.
    height: u32,
    tail: u32,
    /// The leaf each entry is in.
    home: Vec<u32>,
}

impl<const N: usize> Tree<N> {
    /// A tree holding one entry, with id 0. A tree is never empty.
    pub fn new(widths: [u32; 2]) -> Self {
        let mut tree = Tree { leaves: Vec::new(), nodes: Vec::new(), root: 0, height: 1, tail: 0, home: Vec::new() };
        tree.reset(widths);
        tree
    }

    /// Discards everything and starts again with one entry, id 0.
    pub fn reset(&mut self, widths: [u32; 2]) {
        self.leaves.clear();
        self.nodes.clear();
        self.home.clear();

        let mut leaf = Leaf { len: 1, parent: 0, at: 0, next: NONE, slots: [EMPTY; N] };
        leaf.slots[0] = Slot { id: 0, widths };
        self.leaves.push(leaf);
        let mut root = Node::new(NONE);
        root.len = 1;
        root.children[0] = 0;
        root.widths[0][0] = widths[0];
        root.widths[1][0] = widths[1];
        self.nodes.push(root);
        self.home.push(0);
        (self.root, self.height, self.tail) = (0, 1, 0);
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.home.len()
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    pub fn first(&self) -> Cursor {
        let mut node = self.root;
        for _ in 1..self.height {
            node = self.nodes[node as usize].children[0];
        }
        Cursor { leaf: self.nodes[node as usize].children[0], slot: 0 }
    }

    pub fn next(&self, at: Cursor) -> Option<Cursor> {
        let leaf = &self.leaves[at.leaf as usize];
        if at.slot + 1 < leaf.len {
            Some(Cursor { leaf: at.leaf, slot: at.slot + 1 })
        } else if leaf.next != NONE {
            Some(Cursor { leaf: leaf.next, slot: 0 })
        } else {
            None
        }
    }

    pub fn id(&self, at: Cursor) -> Id {
        self.leaves[at.leaf as usize].slots[at.slot as usize].id
    }

    pub fn widths(&self, at: Cursor) -> [u32; 2] {
        self.leaves[at.leaf as usize].slots[at.slot as usize].widths
    }

    /// Where entry `id` is now.
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

    /// Which of two entries comes first.
    pub fn cmp(&self, a: Cursor, b: Cursor) -> Ordering {
        if a.leaf == b.leaf {
            return a.slot.cmp(&b.slot);
        }
        let levels = self.height as usize;
        self.path(a.leaf)[..levels].cmp(&self.path(b.leaf)[..levels])
    }

    /// Child indexes from the root down to `leaf`.
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

    /// The entry holding `offset` of `view`, and how far into that entry it is.
    pub fn seek(&self, view: usize, mut offset: u32) -> (Cursor, u32) {
        let mut node = self.root;
        for _ in 0..self.height {
            let nd = &self.nodes[node as usize];
            let mut child = 0;
            while offset >= nd.widths[view][child] {
                offset -= nd.widths[view][child];
                child += 1;
                assert!(child < nd.len as usize, "offset is past the end");
            }
            node = nd.children[child];
        }
        let leaf = &self.leaves[node as usize];
        let mut slot = 0;
        while offset >= leaf.slots[slot].widths[view] {
            offset -= leaf.slots[slot].widths[view];
            slot += 1;
        }
        (Cursor { leaf: node, slot: slot as u32 }, offset)
    }

    /// The total width, in each view, of everything before an entry.
    pub fn offsets(&self, at: Cursor) -> [u32; 2] {
        let leaf = &self.leaves[at.leaf as usize];
        let mut total = [0; 2];
        for slot in &leaf.slots[..at.slot as usize] {
            total[0] += slot.widths[0];
            total[1] += slot.widths[1];
        }
        let (mut child, mut node) = (leaf.at as usize, leaf.parent);
        loop {
            let nd = &self.nodes[node as usize];
            total[0] += nd.widths[0][..child].iter().sum::<u32>();
            total[1] += nd.widths[1][..child].iter().sum::<u32>();
            if nd.parent == NONE {
                return total;
            }
            (child, node) = (nd.at as usize, nd.parent);
        }
    }

    /// Changes an entry's widths.
    pub fn set_widths(&mut self, at: Cursor, widths: [u32; 2]) {
        let delta = self.replace(at, widths);
        let leaf = &self.leaves[at.leaf as usize];
        let (mut child, mut node) = (leaf.at as usize, leaf.parent);
        loop {
            let nd = &mut self.nodes[node as usize];
            nd.widths[0][child] = nd.widths[0][child].wrapping_add(delta[0]);
            nd.widths[1][child] = nd.widths[1][child].wrapping_add(delta[1]);
            if nd.parent == NONE {
                return;
            }
            (child, node) = (nd.at as usize, nd.parent);
        }
    }

    /// Changes an entry's widths and returns its [`offsets`](Self::offsets), found on the
    /// same pass up the tree.
    pub fn update(&mut self, at: Cursor, widths: [u32; 2]) -> [u32; 2] {
        let delta = self.replace(at, widths);
        self.climb(at, delta)
    }

    /// Stores new widths in an entry's slot and returns the change, as wrapping deltas.
    fn replace(&mut self, at: Cursor, widths: [u32; 2]) -> [u32; 2] {
        let slot = &mut self.leaves[at.leaf as usize].slots[at.slot as usize];
        let delta = [widths[0].wrapping_sub(slot.widths[0]), widths[1].wrapping_sub(slot.widths[1])];
        slot.widths = widths;
        delta
    }

    /// Adds `delta` to the widths recorded for an entry's leaf in each of its ancestors,
    /// and returns the entry's offsets.
    fn climb(&mut self, at: Cursor, delta: [u32; 2]) -> [u32; 2] {
        let leaf = &self.leaves[at.leaf as usize];
        let mut total = [0; 2];
        for slot in &leaf.slots[..at.slot as usize] {
            total[0] += slot.widths[0];
            total[1] += slot.widths[1];
        }
        let (mut child, mut node) = (leaf.at as usize, leaf.parent);
        loop {
            let nd = &mut self.nodes[node as usize];
            for view in 0..2 {
                total[view] += nd.widths[view][..child].iter().sum::<u32>();
                nd.widths[view][child] = nd.widths[view][child].wrapping_add(delta[view]);
            }
            if nd.parent == NONE {
                return total;
            }
            (child, node) = (nd.at as usize, nd.parent);
        }
    }

    /// Adds an entry just before `before`, or at the end for `None`. Returns its id and its
    /// [`offsets`](Self::offsets).
    pub fn insert(&mut self, before: Option<Cursor>, widths: [u32; 2]) -> (Id, [u32; 2]) {
        let before = before.unwrap_or(Cursor { leaf: self.tail, slot: self.leaves[self.tail as usize].len });
        let at = self.make_room(before);
        let id = self.place(at, widths);
        (id, self.climb(at, widths))
    }

    /// Splits an entry in two: a new entry takes `widths` out of the entry at `at`, and
    /// sits just before it if `before`, otherwise just after.
    pub fn split_off(&mut self, at: Cursor, widths: [u32; 2], before: bool) -> Id {
        let at = self.make_room(at);
        let slot = &mut self.leaves[at.leaf as usize].slots[at.slot as usize];
        slot.widths = [slot.widths[0] - widths[0], slot.widths[1] - widths[1]];
        // The leaf's total is unchanged, so no ancestor needs updating.
        self.place(Cursor { leaf: at.leaf, slot: at.slot + !before as u32 }, widths)
    }

    /// Writes a new entry into the slot at `at`, shifting later ones along. The leaf must
    /// have room.
    fn place(&mut self, at: Cursor, widths: [u32; 2]) -> Id {
        let id = self.home.len() as Id;
        self.home.push(at.leaf);
        let leaf = &mut self.leaves[at.leaf as usize];
        let (slot, len) = (at.slot as usize, leaf.len as usize);
        if slot < len {
            leaf.slots.copy_within(slot..len, slot + 1);
        }
        leaf.slots[slot] = Slot { id, widths };
        leaf.len += 1;
        id
    }

    /// Ensures the leaf at `at` has a free slot, splitting it if it is full. Returns where
    /// the slot that was at `at` is now.
    fn make_room(&mut self, at: Cursor) -> Cursor {
        if (self.leaves[at.leaf as usize].len as usize) < N {
            return at;
        }
        let half = N / 2;
        let new = self.leaves.len() as u32;
        let old = &mut self.leaves[at.leaf as usize];
        let mut right = Leaf { len: (N - half) as u32, parent: old.parent, at: 0, next: old.next, slots: [EMPTY; N] };
        right.slots[..N - half].copy_from_slice(&old.slots[half..]);
        old.slots[half..].fill(EMPTY);
        old.len = half as u32;
        old.next = new;
        let parent = old.parent;

        let mut moved = [0; 2];
        for slot in &right.slots[..N - half] {
            self.home[slot.id as usize] = new;
            moved[0] += slot.widths[0];
            moved[1] += slot.widths[1];
        }
        self.leaves.push(right);
        if self.tail == at.leaf {
            self.tail = new;
        }
        self.insert_child(parent, at.leaf, new, moved, 1);

        if at.slot as usize >= half {
            Cursor { leaf: new, slot: at.slot - half as u32 }
        } else {
            at
        }
    }

    /// Adds `new` as the child after `after` in `node`, taking `widths` of what was
    /// recorded for `after` with it. `level` is 1 when the children are leaves.
    fn insert_child(&mut self, mut node: u32, after: u32, new: u32, widths: [u32; 2], level: u32) {
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
        for (of_children, width) in nd.widths.iter_mut().zip(widths) {
            of_children.copy_within(at + 1..len, at + 2);
            of_children[at + 1] = width;
            of_children[at] -= width;
        }
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
        for view in 0..2 {
            right.widths[view][..N - half].copy_from_slice(&old.widths[view][half..]);
        }
        old.len = half as u32;
        let parent = old.parent;
        let moved = [right.total(0), right.total(1)];
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
            for (view, of_children) in top.widths.iter_mut().enumerate() {
                of_children[..2].copy_from_slice(&[old.total(view), moved[view]]);
            }
            self.nodes.push(top);
            self.adopt(level + 1, node, root, 0);
            self.adopt(level + 1, new, root, 1);
            self.root = root;
            self.height += 1;
        } else {
            self.insert_child(parent, node, new, moved, level + 1);
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

    fn index_in_parent(&self, level: u32, child: u32) -> u32 {
        if level == 1 {
            self.leaves[child as usize].at
        } else {
            self.nodes[child as usize].at
        }
    }
}
