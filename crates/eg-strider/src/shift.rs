//! Merging a zone without the tracker, where its concurrent edits keep apart.

use crate::oplog::{OpKind, OpLog};
use crate::replay::Edits;
use std::ops::Range;
use strider_graph::Runs;

/// Zones with more runs than this go straight to the tracker: the sets of runs kept for
/// each run would take too much room.
const MAX_RUNS: usize = 2048;

/// A zone with more clusters than this at once is too scattered for a list of them to be
/// the right tool.
const MAX_CLUSTERS: usize = 32;

/// A stretch of the document that runs of the zone have edited. `start` and `len` are in
/// the document as merged so far.
struct Cluster {
    start: i64,
    len: i64,
    /// The runs that edited here and are not settled, and by how much each changed the
    /// cluster's length.
    members: Vec<(u32, i64)>,
}

impl Cluster {
    fn add(&mut self, run: u32, grew: i64) {
        match self.members.last_mut() {
            Some((last, by)) if *last == run => *by += grew,
            _ => self.members.push((run, grew)),
        }
    }
}

/// The run whose edits are being placed.
struct Viewer<'a> {
    run: u32,
    /// The zone's runs in its version, itself included.
    seen: &'a [u64],
    /// The runs that it and every run after it have seen.
    settled: &'a [u64],
}

fn has(set: &[u64], run: u32) -> bool {
    set[run as usize / 64] >> (run % 64) & 1 == 1
}

#[derive(Default)]
struct Clusters {
    /// In document order.
    list: Vec<Cluster>,
    spare: Vec<Vec<(u32, i64)>>,
}

impl Clusters {
    fn clear(&mut self) {
        for mut cluster in self.list.drain(..) {
            cluster.members.clear();
            self.spare.push(cluster.members);
        }
    }

    /// How much longer cluster `i` is than `viewer` knows, with whether any run it has not
    /// seen edited there. Forgets settled runs first; if none are left, the cluster is
    /// dropped and `None` returned, and `i` is then the cluster that followed.
    fn unseen(&mut self, i: usize, viewer: &Viewer) -> Option<(i64, bool)> {
        let (mut longer, mut any) = (0, false);
        self.list[i].members.retain(|&(run, grew)| {
            if has(viewer.settled, run) {
                return false;
            }
            if !has(viewer.seen, run) {
                longer += grew;
                any = true;
            }
            true
        });
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
        members.push((viewer.run, grew));
        self.list.insert(i, Cluster { start, len, members });
        Some(())
    }

    /// Where an insert of `len` characters at `pos`, in the document as `viewer` knows it,
    /// goes in the document as merged so far.
    fn insert(&mut self, viewer: &Viewer, pos: i64, len: i64) -> Option<i64> {
        let own = !has(viewer.settled, viewer.run);
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
                self.list[i].add(viewer.run, len);
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

    /// Where a removal of the `len` characters at `pos`, in the document as `viewer` knows
    /// it, goes in the document as merged so far.
    fn remove(&mut self, viewer: &Viewer, pos: i64, len: i64) -> Option<i64> {
        let own = !has(viewer.settled, viewer.run);
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
                self.list[i].add(viewer.run, -len);
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

/// Working room for merging zones by shifting.
#[derive(Default)]
pub(crate) struct Shifter {
    /// For each run of the zone: the zone's runs in its version, itself included.
    seen: Vec<u64>,
    /// For each run of the zone: the runs that it and every later run have seen.
    settled: Vec<u64>,
    clusters: Clusters,
}

impl Shifter {
    /// Merges a zone by shifting, adding the edits it comes to to `doc`. Returns false,
    /// with `doc` as it was, if the zone needs the tracker.
    pub fn merge(&mut self, log: &OpLog, runs: &Runs, first_op: &[u32], zone: &Range<usize>, doc: &mut Edits) -> bool {
        if zone.len() > MAX_RUNS {
            return false;
        }
        let words = self.sets(runs, zone);
        let Shifter { seen, settled, clusters } = self;
        clusters.clear();

        let mark = doc.mark();
        for (i, run) in (zone.start..zone.end).enumerate() {
            let viewer = Viewer { run: i as u32, seen: &seen[i * words..][..words], settled: &settled[i * words..][..words] };
            for op in log.ops(first_op[run], runs.events(run)) {
                let at = match op.kind {
                    OpKind::Ins => clusters.insert(&viewer, op.pos as i64, op.len as i64),
                    OpKind::Del => clusters.remove(&viewer, op.pos as i64, op.len as i64),
                };
                match (at, op.kind) {
                    (Some(at), OpKind::Ins) => doc.insert(at as u32, op.text, op.len),
                    (Some(at), OpKind::Del) => doc.remove(at as u32, op.len),
                    (None, _) => {
                        doc.undo_to(mark);
                        return false;
                    }
                }
            }
        }
        true
    }

    /// Fills in `seen` and `settled` for the zone. Returns how many words each set has.
    fn sets(&mut self, runs: &Runs, zone: &Range<usize>) -> usize {
        let count = zone.len();
        let words = count.div_ceil(64);
        self.seen.clear();
        self.seen.resize(count * words, 0);
        for i in 0..count {
            let (earlier, own) = self.seen.split_at_mut(i * words);
            let own = &mut own[..words];
            // Parents from before the zone are before a critical version: all have them.
            for parent in runs.parents(zone.start + i).iter().filter_map(|&p| (p as usize).checked_sub(zone.start)) {
                for (word, theirs) in own.iter_mut().zip(&earlier[parent * words..][..words]) {
                    *word |= theirs;
                }
            }
            own[i / 64] |= 1 << (i % 64);
        }
        self.settled.clear();
        self.settled.resize(count * words, u64::MAX);
        for i in (0..count).rev() {
            for word in 0..words {
                let later = if i + 1 < count { self.settled[(i + 1) * words + word] } else { u64::MAX };
                self.settled[i * words + word] = later & self.seen[i * words + word];
            }
        }
        words
    }
}
