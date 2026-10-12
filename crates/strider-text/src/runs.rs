//! The document at one moment of a backwards replay, described by what becomes of it.

/// In place of a final position: these characters do not survive.
const DOOMED: u32 = u32::MAX;

/// Runs per chunk. Tiny in debug builds, so that tests split and empty chunks often.
const CHUNK: usize = if cfg!(debug_assertions) { 4 } else { 32 };

#[derive(Clone, Copy)]
struct Run {
    len: u32,
    /// Where the run's first character is in the final text, or [`DOOMED`].
    at: u32,
}

impl Run {
    fn with_len(self, len: u32) -> Run {
        Run { len, ..self }
    }

    /// The run without its first `skip` characters.
    fn after(self, skip: u32) -> Run {
        Run { len: self.len - skip, at: if self.at == DOOMED { DOOMED } else { self.at + skip } }
    }

    /// Whether `next`, which follows this run in the document, shares its fate: both
    /// doomed, or both surviving next to each other.
    fn continues_into(self, next: Run) -> bool {
        if self.at == DOOMED || next.at == DOOMED {
            self.at == next.at
        } else {
            self.at + self.len == next.at
        }
    }
}

/// Up to [`CHUNK`] runs: `runs[..before]`, then a gap, then the last `after` of `runs`.
#[derive(Clone, Copy)]
struct Chunk {
    before: usize,
    after: usize,
    /// Characters in the runs before the gap.
    gap_at: u32,
    runs: [Run; CHUNK],
}

impl Chunk {
    const EMPTY: Chunk = Chunk { before: 0, after: 0, gap_at: 0, runs: [Run { len: 0, at: 0 }; CHUNK] };

    fn len(&self) -> usize {
        self.before + self.after
    }

    /// Where run `i` is in `runs`.
    fn slot(&self, i: usize) -> usize {
        if i < self.before { i } else { i + CHUNK - self.len() }
    }

    fn run(&self, i: usize) -> Run {
        self.runs[self.slot(i)]
    }

    fn set(&mut self, i: usize, run: Run) {
        let slot = self.slot(i);
        if i < self.before {
            self.gap_at = self.gap_at - self.runs[slot].len + run.len;
        }
        self.runs[slot] = run;
    }

    /// The run holding the character `offset` characters into the chunk, and how far into
    /// that run it is. One past the last run for the end of the chunk.
    fn find(&self, offset: u32) -> (usize, u32) {
        if offset >= self.gap_at {
            let mut start = self.gap_at;
            for (i, run) in self.runs[CHUNK - self.after..].iter().enumerate() {
                if offset < start + run.len {
                    return (self.before + i, offset - start);
                }
                start += run.len;
            }
            debug_assert_eq!(start, offset);
            return (self.len(), 0);
        }
        // The run just before the gap, then the rest from the start.
        let mut start = self.gap_at - self.runs[self.before - 1].len;
        if offset >= start {
            return (self.before - 1, offset - start);
        }
        start = 0;
        for (i, run) in self.runs[..self.before].iter().enumerate() {
            if offset < start + run.len {
                return (i, offset - start);
            }
            start += run.len;
        }
        unreachable!("offset is before the gap");
    }

    /// Moves the gap so that `i` runs come before it.
    fn move_gap(&mut self, i: usize) {
        let chars = |runs: &[Run]| runs.iter().map(|run| run.len).sum::<u32>();
        if i < self.before {
            let (from, to) = (i, self.before);
            self.gap_at -= chars(&self.runs[from..to]);
            self.after += to - from;
            self.runs.copy_within(from..to, CHUNK - self.after);
        } else if i > self.before {
            let from = CHUNK - self.after;
            let to = from + (i - self.before);
            self.gap_at += chars(&self.runs[from..to]);
            self.after -= to - from;
            self.runs.copy_within(from..to, self.before);
        }
        self.before = i;
    }

    /// Adds a run as run `i`. There must be room.
    fn insert(&mut self, i: usize, run: Run) {
        self.move_gap(i);
        self.runs[i] = run;
        self.before += 1;
        self.gap_at += run.len;
    }

    fn remove(&mut self, i: usize) {
        if i < self.before {
            self.move_gap(i + 1);
            self.before -= 1;
            self.gap_at -= self.runs[i].len;
        } else {
            self.move_gap(i);
            self.after -= 1;
        }
    }
}

pub(crate) struct Runs {
    chunks: Vec<Chunk>,
    /// Chunks no longer in use.
    spare: Vec<u32>,
    /// The chunks in document order, and how many characters each holds.
    order: Vec<u32>,
    totals: Vec<u32>,
    /// The chunk used last: its place in `order`, and the position where it starts.
    near: (usize, u32),
}

impl Runs {
    /// A document of `len` characters, all of which survive: the final text itself.
    pub fn new(len: u32) -> Self {
        let mut first = Chunk::EMPTY;
        if len > 0 {
            first.insert(0, Run { len, at: 0 });
        }
        Runs { chunks: vec![first], spare: Vec::new(), order: vec![0], totals: vec![len], near: (0, 0) }
    }

    /// Puts `len` doomed characters into the document at `pos`.
    pub fn insert_doomed(&mut self, pos: u32, len: u32) {
        let doomed = Run { len, at: DOOMED };
        loop {
            let (k, start) = self.chunk_at(pos);
            let chunk = &mut self.chunks[self.order[k] as usize];
            let (i, offset) = chunk.find(pos - start);
            let room = CHUNK - chunk.len();
            // A doomed run they land in, or next to, takes them in.
            let joined = if offset > 0 {
                Some(i)
            } else if i > 0 && chunk.run(i - 1).at == DOOMED {
                Some(i - 1)
            } else {
                Some(i).filter(|&i| i < chunk.len())
            };
            match joined.map(|i| (i, chunk.run(i))) {
                Some((i, run)) if run.at == DOOMED => chunk.set(i, run.with_len(run.len + len)),
                // In the middle of text that survives, which is now in two parts.
                Some((i, run)) if offset > 0 && room >= 2 => {
                    chunk.set(i, run.with_len(offset));
                    chunk.insert(i + 1, doomed);
                    chunk.insert(i + 2, run.after(offset));
                }
                _ if offset == 0 && room >= 1 => chunk.insert(i, doomed),
                _ => {
                    self.split(k);
                    continue;
                }
            }
            self.totals[k] += len;
            return;
        }
    }

    /// Takes the `len` characters at `pos` out of the document. For each stretch of them
    /// that survives, calls `survivor` with its position in the final text, how far into
    /// the removed characters it starts, and its length.
    pub fn remove(&mut self, pos: u32, len: u32, mut survivor: impl FnMut(u32, u32, u32)) {
        let mut done = 0;
        while done < len {
            let (mut k, mut start) = self.chunk_at(pos);
            if pos == start + self.totals[k] {
                start += self.totals[k];
                k += 1;
                self.near = (k, start);
            }
            let id = self.order[k];
            let chunk = &mut self.chunks[id as usize];
            let (i, offset) = chunk.find(pos - start);
            let run = chunk.run(i);
            let n = (run.len - offset).min(len - done);
            // Surviving text with a piece taken out of its middle becomes two runs: its
            // halves are no longer next to each other in the final text.
            let halved = offset > 0 && offset + n < run.len && run.at != DOOMED;
            if halved && chunk.len() == CHUNK {
                self.split(k);
                continue;
            }
            if run.at != DOOMED {
                survivor(run.at + offset, done, n);
            }
            // `seam`: the run that now starts where the removed characters were.
            let seam = if offset == 0 && n == run.len {
                chunk.remove(i);
                Some(i)
            } else if offset == 0 {
                chunk.set(i, run.after(n));
                Some(i)
            } else if halved {
                chunk.set(i, run.with_len(offset));
                chunk.insert(i + 1, run.after(offset + n));
                None
            } else {
                chunk.set(i, run.with_len(run.len - n));
                (offset + n == run.len).then_some(i + 1)
            };
            done += n;
            self.totals[k] -= n;

            // What was on either side of the removed characters may now be one run.
            if let Some(i) = seam.filter(|&i| done == len && i > 0 && i < chunk.len()) {
                let (left, right) = (chunk.run(i - 1), chunk.run(i));
                if left.continues_into(right) {
                    chunk.set(i - 1, left.with_len(left.len + right.len));
                    chunk.remove(i);
                }
            }
            if chunk.len() == 0 && self.order.len() > 1 {
                self.order.remove(k);
                self.totals.remove(k);
                self.spare.push(id);
                self.near = if k < self.order.len() { (k, start) } else { (k - 1, start - self.totals[k - 1]) };
            }
        }
    }

    /// The chunk that position `pos` is in, or at either end of: its place in `order`, and
    /// the position where it starts.
    fn chunk_at(&mut self, pos: u32) -> (usize, u32) {
        let (mut k, mut start) = self.near;
        while pos < start {
            k -= 1;
            start -= self.totals[k];
        }
        while pos > start + self.totals[k] {
            start += self.totals[k];
            k += 1;
        }
        self.near = (k, start);
        (k, start)
    }

    /// Moves the second half of a chunk's runs into a new chunk after it.
    fn split(&mut self, k: usize) {
        let left = &mut self.chunks[self.order[k] as usize];
        left.move_gap(left.len() / 2);
        let mut right = Chunk::EMPTY;
        right.before = left.after;
        right.runs[..right.before].copy_from_slice(&left.runs[CHUNK - left.after..]);
        right.gap_at = self.totals[k] - left.gap_at;
        left.after = 0;

        let id = match self.spare.pop() {
            Some(id) => {
                self.chunks[id as usize] = right;
                id
            }
            None => {
                self.chunks.push(right);
                self.chunks.len() as u32 - 1
            }
        };
        self.order.insert(k + 1, id);
        self.totals.insert(k + 1, right.gap_at);
        self.totals[k] -= right.gap_at;
    }
}
