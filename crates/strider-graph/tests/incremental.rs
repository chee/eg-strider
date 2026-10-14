use proptest::prelude::*;
use proptest::sample::Index;
use std::ops::Range;
use strider_graph::{BlockIndex, Graph, Segment};

/// The analysis done in one go over the whole history, as it was before it became incremental.
struct Batch {
    entries: Vec<Entry>,
    parents: Vec<u32>,
}

struct Entry {
    start: u32,
    end: u32,
    parents: (u32, u32),
}

struct Cut {
    start: Vec<u32>,
    parent_start: Vec<u32>,
    parents: Vec<u32>,
}

impl Batch {
    fn len(&self) -> u32 {
        self.entries.last().map_or(0, |entry| entry.end)
    }

    fn blocks(&self) -> Cut {
        let n = self.len();

        // A block starts wherever an entry does, and just after every event that is some
        // other event's parent.
        let mut cut = vec![0u64; n as usize / 64 + 1];
        let mut mark = |event: u32| cut[event as usize / 64] |= 1 << (event % 64);
        for e in &self.entries {
            mark(e.start);
        }
        for &p in &self.parents {
            if p + 1 < n {
                mark(p + 1);
            }
        }

        let mut rank = Vec::with_capacity(cut.len());
        let mut count = 0u32;
        for w in &cut {
            rank.push(count);
            count += w.count_ones();
        }
        let mut start = Vec::with_capacity(count as usize + 1);
        for (i, &w) in cut.iter().enumerate() {
            let mut w = w;
            while w != 0 {
                start.push(i as u32 * 64 + w.trailing_zeros());
                w &= w - 1;
            }
        }
        start.push(n);

        // The block containing an event: one less than the number of starts at or before it.
        let block_of = |event: u32| {
            let below = cut[event as usize / 64] & (u64::MAX >> (63 - event % 64));
            rank[event as usize / 64] + below.count_ones() - 1
        };

        let mut parent_start = Vec::with_capacity(count as usize + 1);
        let mut parents = Vec::with_capacity(self.parents.len() + count as usize);
        let mut block = 0usize;
        for e in &self.entries {
            debug_assert_eq!(start[block], e.start);
            parent_start.push(parents.len() as u32);
            parents.extend(self.parents[e.parents.0 as usize..e.parents.1 as usize].iter().map(|&p| block_of(p)));
            block += 1;
            // Later pieces of the same entry each continue from the piece before.
            while start[block] < e.end {
                parent_start.push(parents.len() as u32);
                parents.push(block as u32 - 1);
                block += 1;
            }
        }
        parent_start.push(parents.len() as u32);

        Cut { start, parent_start, parents }
    }
}

impl Cut {
    fn len(&self) -> usize {
        self.parent_start.len() - 1
    }

    fn parents(&self, block: usize) -> &[u32] {
        &self.parents[self.parent_start[block] as usize..self.parent_start[block + 1] as usize]
    }

    fn segments(&self) -> Vec<Segment> {
        let blocks = self.len();
        let start = &self.start;

        // seen[i]: a `q` such that block i's first event has all of events 0..q in its
        // history. Exact when the block has seen everything before it (its parents are then
        // exactly the heads of the graph so far), otherwise a lower bound from its parents.
        let mut seen = vec![0u32; blocks];
        let mut is_head = vec![false; blocks];
        let mut heads = 0usize;
        for i in 0..blocks {
            let parents = self.parents(i);
            seen[i] = if parents.len() == heads && parents.iter().all(|&p| is_head[p as usize]) {
                start[i]
            } else {
                let seen_after = |p: usize| if seen[p] == start[p] { start[p + 1] } else { seen[p] };
                parents.iter().map(|&p| seen_after(p as usize)).max().unwrap_or(0)
            };
            for &p in parents {
                if std::mem::take(&mut is_head[p as usize]) {
                    heads -= 1;
                }
            }
            is_head[i] = true;
            heads += 1;
        }

        // The version just before block i is critical when every block from i on has seen all
        // of events 0..start[i].
        let mut least_seen = vec![u32::MAX; blocks + 1];
        for i in (0..blocks).rev() {
            least_seen[i] = least_seen[i + 1].min(seen[i]);
        }
        let critical = |i: usize| least_seen[i] >= start[i];

        let mut segments = Vec::new();
        let mut i = 0;
        while i < blocks {
            debug_assert!(critical(i));
            let mut end = i + 1;
            if critical(end) {
                // Critical on both sides: nothing is concurrent with this block. Take every
                // block that follows in the same way.
                while end < blocks && critical(end + 1) {
                    end += 1;
                }
                segments.push(Segment::Linear(i as BlockIndex..end as BlockIndex));
            } else {
                while !critical(end) {
                    end += 1;
                }
                segments.push(Segment::Region(i as BlockIndex..end as BlockIndex));
            }
            i = end;
        }
        segments
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(600))]

    /// Checks the graph's running analysis against a fresh one. Events come in chains of 1 to
    /// 4, each after one or two earlier events, often the last, so that histories have long
    /// linear stretches, forks partway along and merges.
    #[test]
    fn running_analysis_matches_a_fresh_one(steps in proptest::collection::vec((any::<[Index; 2]>(), 0u8..10, 1u32..5), 1..120)) {
        let mut graph = Graph::new();
        let mut batch = Batch { entries: Vec::new(), parents: Vec::new() };
        for ([a, b], kind, len) in steps {
            let total = graph.len();
            let mut parents: Vec<u32> = match (total, kind) {
                (0, _) => vec![],
                (_, 0..=4) => vec![total - 1],
                (_, 5..=7) => vec![a.index(total as usize) as u32],
                _ => vec![a.index(total as usize) as u32, b.index(total as usize) as u32],
            };
            parents.sort_unstable();
            parents.dedup();
            // The frontier decides what a later event may leave unseen; keep some merges whole.
            if kind == 9 {
                parents = graph.frontier().to_vec();
            }
            let continues = graph.continues(&parents);
            let events: Range<u32> = graph.push(&parents, len);
            if continues {
                batch.entries.last_mut().unwrap().end = events.end;
            } else {
                let at = batch.parents.len() as u32;
                batch.parents.extend_from_slice(&parents);
                batch.entries.push(Entry { start: events.start, end: events.end, parents: (at, batch.parents.len() as u32) });
            }

            let (cut, blocks) = (batch.blocks(), graph.blocks());
            prop_assert_eq!(blocks.len() as usize, cut.len());
            for block in 0..cut.len() {
                prop_assert_eq!(blocks.events(block as BlockIndex), cut.start[block]..cut.start[block + 1]);
                prop_assert_eq!(blocks.parents(block as BlockIndex), cut.parents(block));
            }
            prop_assert_eq!(graph.segments(), cut.segments());
        }
    }
}
