//! Merges a region without CRDT state when its concurrent edits do not touch. This is the
//! simple case of operational transformation: an edit's position is moved by the net length
//! of the concurrent edits before it. Edited stretches of the document are kept as
//! clusters, separated by text every remaining block has seen. An edit landing in a cluster
//! that holds edits it has not seen cannot be placed this way, and the region goes to the
//! tracker. A block is stable once every later block has seen it (causal stability).

use crate::oplog::{OpIndex, OpKind, OpLog};
use crate::replay::Placed;
use std::ops::Range;
use strider_graph::Blocks;

/// Regions with more blocks than this go straight to the tracker. The sets of blocks kept for
/// each block grow with the square of this: 256 KB here.
const MAX_BLOCKS: usize = 1024;

/// With more clusters than this at once, looking through the list for every edit costs
/// more than the tracker would.
const MAX_CLUSTERS: usize = 64;

/// A stretch of the document that blocks of the region have edited. `start` and `len` are in
/// the document as merged so far.
struct Cluster {
    start: i64,
    len: i64,
    /// The blocks that edited here and are not stable, and by how much each changed the
    /// cluster's length.
    members: Vec<(u32, i64)>,
}

impl Cluster {
    /// Records that `block` changed the cluster's length by `grew`.
    fn add(&mut self, block: u32, grew: i64) {
        match self.members.last_mut() {
            Some((last, by)) if *last == block => *by += grew,
            _ => self.members.push((block, grew)),
        }
    }
}

/// The block whose edits are being placed.
struct Viewer<'a> {
    block: u32,
    /// The region's blocks in its version, itself included.
    seen: &'a [u64],
    /// The blocks that it and every block after it have seen.
    stable: &'a [u64],
}

/// Tells whether `block` is in `set`.
fn has(set: &[u64], block: u32) -> bool {
    set[block as usize / 64] >> (block % 64) & 1 == 1
}

#[derive(Default)]
struct Clusters {
    /// In document order.
    list: Vec<Cluster>,
    spare: Vec<Vec<(u32, i64)>>,
}

impl Clusters {
    /// Empties the list, keeping its allocations for the next region.
    fn clear(&mut self) {
        for mut cluster in self.list.drain(..) {
            cluster.members.clear();
            self.spare.push(cluster.members);
        }
    }

    /// Works out how much longer cluster `i` is than `viewer` knows, with whether any block it has not
    /// seen edited there. Forgets stable blocks first; if none are left, the cluster is
    /// dropped and `None` returned, and `i` is then the cluster that followed.
    fn unseen(&mut self, i: usize, viewer: &Viewer) -> Option<(i64, bool)> {
        let (mut longer, mut any, mut stale) = (0, false, false);
        for &(block, grew) in &self.list[i].members {
            if has(viewer.stable, block) {
                stale = true;
            } else if !has(viewer.seen, block) {
                longer += grew;
                any = true;
            }
        }
        // Rewriting the list only when something has to go keeps the common case to a read.
        if stale {
            self.list[i].members.retain(|&(block, _)| !has(viewer.stable, block));
        }
        if self.list[i].members.is_empty() {
            self.spare.push(self.list.remove(i).members);
            return None;
        }
        Some((longer, any))
    }

    /// Adds a cluster as cluster `i`, unless there are too many.
    fn open(&mut self, i: usize, start: i64, len: i64, viewer: &Viewer, grew: i64) -> Option<()> {
        if self.list.len() == MAX_CLUSTERS {
            return None;
        }
        let mut members = self.spare.pop().unwrap_or_default();
        members.push((viewer.block, grew));
        self.list.insert(i, Cluster { start, len, members });
        Some(())
    }

    /// Finds where an insert of `len` characters at `pos`, in the document as `viewer` knows it,
    /// goes in the document as merged so far.
    fn insert(&mut self, viewer: &Viewer, pos: i64, len: i64) -> Option<i64> {
        let own = !has(viewer.stable, viewer.block);
        // `shift`: how much further along the merged document is than the viewer's.
        let (mut i, mut shift, mut inside) = (0, 0, false);
        while i < self.list.len() {
            let Some((longer, any)) = self.unseen(i, viewer) else { continue };
            // The cluster as the viewer knows it.
            let start = self.list[i].start - shift;
            let end = start + self.list[i].len - longer;
            if pos < start {
                break;
            }
            if pos <= end {
                if any {
                    return None;
                }
                inside = true;
                break;
            }
            shift += longer;
            i += 1;
        }
        let at = pos + shift;
        if inside {
            self.list[i].len += len;
            if own {
                self.list[i].add(viewer.block, len);
            }
            i += 1;
        } else if own {
            self.open(i, at, len, viewer, len)?;
            i += 1;
        }
        for later in &mut self.list[i..] {
            later.start += len;
        }
        Some(at)
    }

    /// Finds where a removal of the `len` characters at `pos`, in the document as `viewer` knows
    /// it, goes in the document as merged so far.
    fn remove(&mut self, viewer: &Viewer, pos: i64, len: i64) -> Option<i64> {
        let own = !has(viewer.stable, viewer.block);
        // Clusters that end before the removed text, with a character in between.
        let (mut i, mut shift) = (0, 0);
        while i < self.list.len() {
            let Some((longer, _)) = self.unseen(i, viewer) else { continue };
            if self.list[i].start - shift + self.list[i].len - longer >= pos {
                break;
            }
            shift += longer;
            i += 1;
        }
        // Clusters `i..j` that the removed text reaches into or touches.
        let (mut j, mut shift_at_start, mut shift_at_end) = (i, shift, shift);
        while j < self.list.len() {
            let Some((longer, any)) = self.unseen(j, viewer) else { continue };
            let start = self.list[j].start - shift_at_end;
            let end = start + self.list[j].len - longer;
            if start > pos + len {
                break;
            }
            if end == pos {
                // Touches the removed text from before: all of it lies before.
                shift_at_start = shift_at_end + longer;
            } else if start < pos + len && any {
                return None;
            }
            shift_at_end += longer;
            j += 1;
        }
        let at = pos + shift_at_start;
        if i < j {
            // The removed text and everything it touches are one cluster from now on.
            let start = at.min(self.list[i].start);
            let end = (pos + len + shift_at_end).max(self.list[j - 1].start + self.list[j - 1].len);
            for _ in i + 1..j {
                let mut absorbed = self.list.remove(i + 1).members;
                self.list[i].members.extend_from_slice(&absorbed);
                absorbed.clear();
                self.spare.push(absorbed);
            }
            self.list[i].start = start;
            self.list[i].len = end - start - len;
            if own {
                self.list[i].add(viewer.block, -len);
            }
            i += 1;
        } else if own {
            self.open(i, at, 0, viewer, -len)?;
            i += 1;
        }
        for later in &mut self.list[i..] {
            later.start -= len;
        }
        Some(at)
    }
}

/// Working room for merging regions by shifting.
#[derive(Default)]
pub(crate) struct Shifter {
    /// For each block of the region: the region's blocks in its version, itself included.
    seen: Vec<u64>,
    /// For each block of the region: the blocks that it and every later block have seen.
    stable: Vec<u64>,
    clusters: Clusters,
}

impl Shifter {
    /// Merges a region by shifting, recording in `doc` where its ops go. Returns false, with
    /// `doc` as it was, if the region needs the tracker.
    pub fn merge(&mut self, log: &OpLog, blocks: &Blocks, first_op: &[OpIndex], region: &Range<usize>, doc: &mut Placed) -> bool {
        if region.len() > MAX_BLOCKS {
            return false;
        }
        let words = self.sets(blocks, region);
        let Shifter { seen, stable, clusters } = self;
        clusters.clear();

        let mark = doc.mark();
        for (i, block) in (region.start..region.end).enumerate() {
            let viewer = Viewer { block: i as u32, seen: &seen[i * words..][..words], stable: &stable[i * words..][..words] };
            doc.begin(block..block + 1);
            for op in log.ops(first_op[block]..first_op[block + 1], blocks.events(block)) {
                let at = match op.kind {
                    OpKind::Ins => clusters.insert(&viewer, op.pos as i64, op.len as i64),
                    OpKind::Del => clusters.remove(&viewer, op.pos as i64, op.len as i64),
                };
                match (at, op.kind) {
                    (Some(at), OpKind::Ins) => doc.insert(at as u32, op.len),
                    (Some(at), OpKind::Del) => {
                        doc.remove(at as u32, op.len);
                        doc.end_delete();
                    }
                    (None, _) => {
                        doc.undo_to(mark);
                        return false;
                    }
                }
            }
        }
        true
    }

    /// Fills in `seen` and `stable` for the region. Returns how many words each set has.
    fn sets(&mut self, blocks: &Blocks, region: &Range<usize>) -> usize {
        let count = region.len();
        let words = count.div_ceil(64);
        self.seen.clear();
        self.stable.clear();
        if words == 1 {
            // Most regions are this small: each set is one word, with no slices to cut.
            for i in 0..count {
                let inside = blocks.parents(region.start + i).iter().filter_map(|&p| (p as usize).checked_sub(region.start));
                let seen = inside.fold(1 << i, |seen, parent| seen | self.seen[parent]);
                self.seen.push(seen);
            }
            self.stable.resize(count, 0);
            let mut later = u64::MAX;
            for i in (0..count).rev() {
                later &= self.seen[i];
                self.stable[i] = later;
            }
            return words;
        }
        self.seen.resize(count * words, 0);
        for i in 0..count {
            let (earlier, own) = self.seen.split_at_mut(i * words);
            let own = &mut own[..words];
            // Parents from before the region are before a critical version: all have them.
            for parent in blocks.parents(region.start + i).iter().filter_map(|&p| (p as usize).checked_sub(region.start)) {
                for (word, theirs) in own.iter_mut().zip(&earlier[parent * words..][..words]) {
                    *word |= theirs;
                }
            }
            own[i / 64] |= 1 << (i % 64);
        }
        self.stable.resize(count * words, u64::MAX);
        for i in (0..count).rev() {
            for word in 0..words {
                let later = if i + 1 < count { self.stable[(i + 1) * words + word] } else { u64::MAX };
                self.stable[i * words + word] = later & self.seen[i * words + word];
            }
        }
        words
    }
}
