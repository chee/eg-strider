use crate::oplog::{OpIndex, OpKind, OpLog};
use crate::replay::Placed;
use std::ops::Range;
use strider_graph::{BlockIndex, Blocks};

/// Regions with more blocks go straight to the tracker. The sets kept per block grow with its
/// square: 256 KB here.
const MAX_BLOCKS: usize = 1024;

/// Beyond this many clusters, scanning them for every edit costs more than the tracker would.
const MAX_CLUSTERS: usize = 64;

/// A block's place among its region's blocks, counting from 0.
type BlockInRegion = u32;

/// A set of a region's blocks, one bit each, 64 to a word.
type BlockSet = [u64];

/// A stretch of the document that the region's blocks have edited, measured in the document as
/// merged so far.
struct Cluster {
    start: i64,
    len: i64,
    /// The blocks that edited here and are not stable, and how much each changed its length.
    members: Vec<(BlockInRegion, i64)>,
}

impl Cluster {
    /// Records that `block` changed the cluster's length by `grew`.
    fn add(&mut self, block: BlockInRegion, grew: i64) {
        match self.members.last_mut() {
            Some((last, by)) if *last == block => *by += grew,
            _ => self.members.push((block, grew)),
        }
    }
}

/// The block whose edits are being placed.
struct Viewer<'a> {
    block: BlockInRegion,
    /// The region's blocks in its version, itself included.
    seen: &'a BlockSet,
    /// The blocks that it and every later block have seen.
    stable: &'a BlockSet,
}

/// Tells whether `block` is in `set`.
fn has(set: &BlockSet, block: BlockInRegion) -> bool {
    set[block as usize / 64] >> (block % 64) & 1 == 1
}

#[derive(Default)]
struct Clusters {
    /// In document order.
    list: Vec<Cluster>,
    spare: Vec<Vec<(BlockInRegion, i64)>>,
}

impl Clusters {
    /// Empties the list, keeping its allocations for the next region.
    fn clear(&mut self) {
        for mut cluster in self.list.drain(..) {
            cluster.members.clear();
            self.spare.push(cluster.members);
        }
    }

    /// Returns how much longer cluster `i` is than `viewer` knows, and whether any block it has
    /// not seen edited there. Stable blocks are forgotten first; a cluster left empty is
    /// dropped and `None` returned, with `i` then naming the next cluster.
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

    /// Adds a cluster at `i`, unless there are too many.
    fn open(&mut self, i: usize, start: i64, len: i64, viewer: &Viewer, grew: i64) -> Option<()> {
        if self.list.len() == MAX_CLUSTERS {
            return None;
        }
        let mut members = self.spare.pop().unwrap_or_default();
        members.push((viewer.block, grew));
        self.list.insert(i, Cluster { start, len, members });
        Some(())
    }

    /// Places an insert of `len` characters at `pos`, in the document as `viewer` knows it, in
    /// the document as merged so far.
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

    /// Places a removal of the `len` characters at `pos`, in the document as `viewer` knows it,
    /// in the document as merged so far.
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

/// Merges a region without CRDT state when its concurrent edits do not touch: the simple case
/// of operational transformation, moving each edit by the net length of the concurrent edits
/// before it. Edited stretches are kept as clusters, and an edit landing in a cluster that
/// holds edits it has not seen sends the region to the tracker. A block is stable once every
/// later block has seen it.
#[derive(Default)]
pub(crate) struct Shifter {
    /// Per block of the region: the region's blocks in its version, itself included.
    seen: Vec<u64>,
    /// Per block of the region: the blocks that it and every later block have seen.
    stable: Vec<u64>,
    clusters: Clusters,
}

impl Shifter {
    /// Merges a region by shifting, recording in `document` where its ops go. Returns false,
    /// leaving `document` as it was, if the region needs the tracker.
    pub fn merge(&mut self, log: &OpLog, blocks: &Blocks, first_op: &[OpIndex], region: &Range<BlockIndex>, document: &mut Placed) -> bool {
        if region.len() > MAX_BLOCKS {
            return false;
        }
        let words = self.sets(blocks, region);
        let Shifter { seen, stable, clusters } = self;
        clusters.clear();

        let mark = document.mark();
        for (i, block) in (region.start..region.end).enumerate() {
            let viewer = Viewer { block: i as BlockInRegion, seen: &seen[i * words..][..words], stable: &stable[i * words..][..words] };
            document.begin(block..block + 1);
            for op in log.ops(first_op[block as usize]..first_op[block as usize + 1], blocks.events(block)) {
                let at = match op.kind {
                    OpKind::Ins => clusters.insert(&viewer, op.pos as i64, op.len as i64),
                    OpKind::Del => clusters.remove(&viewer, op.pos as i64, op.len as i64),
                };
                match (at, op.kind) {
                    (Some(at), OpKind::Ins) => document.insert(at as u32, op.len),
                    (Some(at), OpKind::Del) => {
                        document.remove(at as u32, op.len);
                        document.end_delete();
                    }
                    (None, _) => {
                        document.undo_to(mark);
                        return false;
                    }
                }
            }
        }
        true
    }

    /// Fills in `seen` and `stable` for the region. Returns how many words each set has.
    fn sets(&mut self, blocks: &Blocks, region: &Range<BlockIndex>) -> usize {
        let count = region.len();
        let words = count.div_ceil(64);
        self.seen.clear();
        self.stable.clear();
        if words == 1 {
            // Most regions are this small: each set is one word, with no slices to cut.
            for i in 0..count {
                let inside = blocks.parents(region.start + i as BlockIndex).iter().filter_map(|&p| p.checked_sub(region.start)).map(|p| p as usize);
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
            for parent in blocks.parents(region.start + i as BlockIndex).iter().filter_map(|&p| p.checked_sub(region.start)).map(|p| p as usize) {
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
