//! A saved document, laid out so that opening it reads little more than its text:
//!
//! - a header: the agents and how far each has counted, how many events the history has, its
//!   heads, the length of the text, and how many events and bytes each block of history has;
//! - the text of the document with every event merged;
//! - the history in blocks of events, each starting at a critical version, so that a block
//!   and those after it can be read without the ones before.
//!
//! Loading reads the header and the text, and keeps the history as it is until an event
//! arrives that was made at an older version. Then it reads the blocks from the one holding
//! that version on. A block holds its events in columns, one for each field, so that a
//! field's values sit together and pack well:
//!
//! - a number is a LEB128 varint, and a column is its length in bytes and then its values;
//! - a column whose values mostly repeat is stored as runs: a value, then how many times;
//! - an op's position is stored as how far it is from where the op before left offset, and a
//!   parent as how far back it is;
//! - the columns of a block, the text it inserts and the document's text are each compressed
//!   with Zstandard.

use super::{AgentRun, Op, OpKind, OpLog, OpSlice};
use crate::replay::Base;
use std::borrow::Cow;
use std::ops::Range;
use strider_graph::{BlockId, EventId, Graph, Segment};

const MAGIC: &[u8] = b"eg-strider\x05";

/// About how many events go in the last block of saved history. Each block before it is twice
/// the size of the one after: recent history, which is what an event made at an older version
/// most often needs, can be read without much else, and the rest packs well in a few large
/// blocks. Tiny in debug builds, so that tests on short histories save several.
const BLOCK: u32 = if cfg!(debug_assertions) { 8 } else { 1 << 15 };

/// A block of saved history's place among the saved blocks.
pub(crate) type SavedBlockIndex = usize;

/// A file that is not a saved document, or is a damaged one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Malformed;

impl std::fmt::Display for Malformed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("not a saved eg-strider document")
    }
}

impl std::error::Error for Malformed {}

/// History that is still as it was saved: the events before the first that the log holds.
pub(crate) struct Packed {
    /// The file the log was loaded from.
    bytes: Vec<u8>,
    /// Where each block of that history is in `bytes`.
    blocks: Vec<Range<usize>>,
    /// The first event of each block, then the number of events in them all.
    starts: Vec<EventId>,
    /// The events of that history that none of its others descends from.
    pub heads: Vec<EventId>,
}

impl Packed {
    /// Finds the block holding an event.
    pub fn block_holding(&self, event: EventId) -> SavedBlockIndex {
        self.starts.partition_point(|&start| start <= event) - 1
    }
}

impl OpLog {
    /// Writes out the log with the document it merges to, as a file that [`load`](Self::load)
    /// opens without merging anything.
    pub fn save(&mut self) -> Vec<u8> {
        self.checkout();
        let (floor, len) = (self.graph.floor(), self.graph.len());
        // History that was never read is written as it is; the rest is cut into blocks.
        let kept = self.packed.iter().flat_map(|packed| {
            packed.blocks.iter().zip(packed.starts.windows(2)).map(|(block, ends)| (Cow::from(&packed.bytes[block.clone()]), ends[1] - ends[0]))
        });
        let cuts = self.cuts(floor, len);
        let new = cuts.windows(2).map(|ends| (Cow::from(self.pack(ends[0]..ends[1])), ends[1] - ends[0]));
        let blocks: Vec<(Cow<[u8]>, u32)> = kept.chain(new).collect();
        let document = self.merged();
        let text = squeezed(document.text.as_bytes());

        let mut out = MAGIC.to_vec();
        number(&mut out, self.agent_names.len());
        for (name, &next) in self.agent_names.iter().zip(&self.next_sequence_numbers) {
            number(&mut out, name.len());
            out.extend_from_slice(name.as_bytes());
            number(&mut out, next as usize);
        }
        for part in [len, document.chars] {
            number(&mut out, part as usize);
        }
        let heads = self.graph.frontier();
        number(&mut out, heads.len());
        heads.iter().for_each(|&head| number(&mut out, (len - head) as usize));
        number(&mut out, document.text.len());
        number(&mut out, text.len());
        number(&mut out, blocks.len());
        for (block, events) in &blocks {
            number(&mut out, block.len());
            number(&mut out, *events as usize);
        }
        out.extend_from_slice(&text);
        blocks.iter().for_each(|(block, _)| out.extend_from_slice(block));
        out
    }

    /// Opens a file that [`save`](Self::save) wrote: the log, ready for more events, with its
    /// document. The history is checked only when it is read, which is when an event arrives
    /// that was made at an older version; if that part of the file is damaged, reading it
    /// panics.
    pub fn load(bytes: Vec<u8>) -> Result<OpLog, Malformed> {
        let mut file = Reader(bytes.strip_prefix(MAGIC).ok_or(Malformed)?);
        let mut log = OpLog::new();
        for _ in 0..file.count()? {
            let len = file.count()?;
            log.agent_names.push(std::str::from_utf8(file.take(len)).map_err(|_| Malformed)?.to_string());
            log.next_sequence_numbers.push(file.number()?);
        }
        let (events, chars) = (file.number()?, file.number()?);
        let heads = (0..file.count()?).map(|_| events.checked_sub(file.number()?).ok_or(Malformed)).collect::<Result<Vec<_>, _>>()?;
        let in_order = heads.windows(2).all(|pair| pair[0] < pair[1]) && heads.last().map_or(events == 0, |&head| head + 1 == events);
        let (text_len, text_packed) = (file.number()? as usize, file.number()? as usize);
        let (mut starts, mut lens, mut end) = (vec![0], Vec::new(), 0 as EventId);
        for _ in 0..file.count()? {
            lens.push(file.number()? as usize);
            end = end.checked_add(file.number()?).filter(|&next| next > end).ok_or(Malformed)?;
            starts.push(end);
        }

        // The parts follow the header, and together are the rest of the file.
        let mut at = bytes.len() - file.0.len();
        let mut part = |len: usize| {
            let end = at.checked_add(len).filter(|&end| end <= bytes.len()).ok_or(Malformed)?;
            Ok(std::mem::replace(&mut at, end)..end)
        };
        let text = part(text_packed)?;
        let blocks = lens.into_iter().map(&mut part).collect::<Result<Vec<_>, Malformed>>()?;
        if at != bytes.len() || !in_order || end != events {
            return Err(Malformed);
        }
        let text = String::from_utf8(unpacked(&bytes[text], text_len)?).map_err(|_| Malformed)?;
        if text.chars().count() != chars as usize {
            return Err(Malformed);
        }

        log.graph = Graph::after(events, &heads);
        log.base = Base { events, chars, text };
        log.packed = (events > 0).then_some(Packed { bytes, blocks, starts, heads });
        Ok(log)
    }

    /// Reads the history that is still as it was saved from block `from` on, so that the log
    /// holds every event since that block's first.
    pub(crate) fn unpack_from(&mut self, from: SavedBlockIndex) {
        let Some(mut packed) = self.packed.take() else { return };
        let later = std::mem::take(self);
        (self.agent_names, self.next_sequence_numbers, self.memo) = (later.agent_names, later.next_sequence_numbers, later.memo);
        (self.base, self.latest) = (later.base, later.latest);
        self.memo.outdate();
        self.graph = Graph::after(packed.starts[from], &[]);
        // The first event of a block has every head of the history before it for parents.
        let mut heads = Vec::new();
        for (i, block) in packed.blocks[from..].iter().enumerate() {
            let first = self.unpack(&packed.bytes[block.clone()]).expect("the saved history is damaged");
            if i == 0 {
                heads = first;
            }
        }

        let shift = self.content.len() as u32;
        self.content.push_str(&later.content);
        self.ops.extend(later.ops.into_iter().map(|op| Op { content: (op.content.0 + shift, op.content.1 + shift), ..op }));
        self.agent_runs.extend(later.agent_runs);
        self.totals = (self.totals.0 + later.totals.0, self.totals.1 + later.totals.1);
        let blocks = later.graph.blocks();
        for block in 0..blocks.len() {
            let parents: Vec<EventId> = blocks.parents(block).iter().map(|&parent| blocks.start(parent + 1) - 1).collect();
            // A block of the later events with no block for a parent followed the old heads.
            let parents = if parents.is_empty() { &packed.heads } else { &parents };
            self.graph.push(parents, blocks.events(block).len() as u32);
        }

        packed.blocks.truncate(from);
        packed.starts.truncate(from + 1);
        packed.heads = heads;
        self.packed = (from > 0).then_some(packed);
    }

    /// Returns the events that the chain of block `block` starts from: its parents, which for
    /// a chain with no block before it are the heads of the history still as it was saved.
    fn parents_of(&self, block: BlockId) -> Vec<EventId> {
        let blocks = self.graph.blocks();
        let parents: Vec<EventId> = blocks.parents(block).iter().map(|&parent| blocks.start(parent + 1) - 1).collect();
        match &self.packed {
            Some(packed) if parents.is_empty() => packed.heads.clone(),
            _ => parents,
        }
    }

    /// Chooses where to cut the events `from..to`, which the log holds, into blocks: at
    /// critical versions, about [`BLOCK`] events before the end and twice as far apart each
    /// time. Returns `from` and then the end of each block.
    fn cuts(&self, from: EventId, to: EventId) -> Vec<EventId> {
        let blocks = self.graph.blocks();
        let (mut cuts, mut size) = (vec![to], BLOCK);
        for segment in self.graph.segments().iter().rev() {
            let stretch = segment.blocks();
            let (start, end) = (blocks.start(stretch.start).max(from), blocks.start(stretch.end).min(to));
            if start >= end {
                continue;
            }
            // Anywhere in a linear stretch is a critical version; a region has to stay whole.
            let mut last = *cuts.last().expect("the last cut");
            while matches!(segment, Segment::Linear(_)) && last - start > size {
                last -= size;
                cuts.push(last);
                size *= 2;
            }
            if last - start >= size {
                cuts.push(start);
                size *= 2;
            }
        }
        if *cuts.last().expect("the last cut") > from {
            cuts.push(from);
        }
        cuts.reverse();
        cuts
    }

    /// Packs the events `events`, which the log holds, as a block.
    fn pack(&self, events: Range<EventId>) -> Vec<u8> {
        let mut columns = Vec::new();

        // The runs of events by one agent. An agent's runs nearly always carry on from one
        // another, so after its first in the block a run's sequence number is stored as how
        // far it is from that.
        let first = self.agent_runs.partition_point(|run| run.first_event + run.len <= events.start);
        let agent_runs = || {
            self.agent_runs[first..].iter().take_while(|run| run.first_event < events.end).map(|run| {
                let (start, end) = (run.first_event.max(events.start), (run.first_event + run.len).min(events.end));
                (run.agent, run.sequence_number + (start - run.first_event), end - start)
            })
        };
        column(&mut columns, |out| {
            number(out, agent_runs().count());
            agent_runs().for_each(|(agent, ..)| number(out, agent as usize));
        });
        let mut next = vec![0; self.agent_names.len()];
        column(&mut columns, |out| {
            runs(
                out,
                agent_runs().map(|(agent, sequence_number, len)| {
                    zigzag(i64::from(sequence_number) - i64::from(std::mem::replace(&mut next[agent as usize], sequence_number + len)))
                }),
            )
        });
        column(&mut columns, |out| agent_runs().for_each(|(.., len)| number(out, len as usize)));

        // The ops: `len << 4 | trims << 3 | wide << 2 | backspace << 1 | delete`, the position,
        // and for a wide insert (one with characters longer than a byte) the length of its
        // text in bytes.
        let ops = || self.ops(self.op_containing(events.start)..self.op_containing(events.end), events.clone());
        let bytes = |op: &OpSlice| (op.text.1 - op.text.0) as usize;
        let wide = |op: &OpSlice| op.kind == OpKind::Ins && bytes(op) != op.len as usize;
        column(&mut columns, |out| {
            number(out, ops().count());
            // A delete only trims an insert that is in the block with it.
            let mut open = false;
            for op in ops() {
                let trims = op.trims && open;
                open = op.kind == OpKind::Ins || trims;
                let flags = usize::from(trims) << 3 | usize::from(wide(&op)) << 2 | usize::from(!op.forward) << 1 | usize::from(op.kind == OpKind::Del);
                number(out, (op.len as usize) << 4 | flags);
            }
        });
        column(&mut columns, |out| {
            let mut cursor = 0;
            for op in ops() {
                number(out, zigzag(i64::from(op.pos) - i64::from(std::mem::replace(&mut cursor, after(op.kind, op.pos, op.len)))) as usize);
            }
        });
        column(&mut columns, |out| ops().filter(wide).for_each(|op| number(out, bytes(&op))));

        // The graph, as chains of events: how long each is, and its first event's parents as
        // how far back they are. A chain cut offset from its start by the block follows the event
        // before it, which is stored as one parent fewer than none.
        let blocks = self.graph.blocks();
        let chains = || (blocks.containing(events.start)..blocks.len()).take_while(|&block| blocks.start(block) < events.end);
        let whole = |block: &BlockId| blocks.start(*block) >= events.start;
        column(&mut columns, |out| {
            number(out, chains().count());
            for block in chains() {
                number(out, (blocks.start(block + 1).min(events.end) - blocks.start(block).max(events.start)) as usize);
            }
        });
        column(&mut columns, |out| runs(out, chains().map(|block| if whole(&block) { self.parents_of(block).len() as u64 + 1 } else { 0 })));
        column(&mut columns, |out| {
            for block in chains().filter(whole) {
                self.parents_of(block).iter().for_each(|&parent| number(out, (blocks.start(block) - parent) as usize));
            }
        });

        let text: String = ops().map(|op| &self.content[op.text.0 as usize..op.text.1 as usize]).collect();
        let mut out = Vec::new();
        for part in [&columns[..], text.as_bytes()] {
            let packed = squeezed(part);
            number(&mut out, part.len());
            number(&mut out, packed.len());
            out.extend_from_slice(&packed);
        }
        out
    }

    /// Reads a block that [`pack`](Self::pack) wrote, whose events come straight after those
    /// the log holds, and adds them. Returns the parents of the first.
    fn unpack(&mut self, block: &[u8]) -> Result<Vec<EventId>, Malformed> {
        let mut file = Reader(block);
        let columns = file.packed()?;
        let text = String::from_utf8(file.packed()?).map_err(|_| Malformed)?;
        let mut columns = Reader(&columns);
        let start = self.graph.len();

        let (mut agents, mut ahead, mut lens) = (columns.column()?, Runs::of(columns.column()?), columns.column()?);
        let agent_runs = agents.count()?;
        self.agent_runs.reserve(agent_runs);
        let mut next = vec![0u32; self.agent_names.len()];
        let mut events = start;
        for _ in 0..agent_runs {
            let (agent, len) = (agents.number()?, lens.number()?);
            let expected = next.get_mut(agent as usize).ok_or(Malformed)?;
            let sequence_number = u32::try_from(i64::from(*expected) + unzigzag(ahead.next()?)).map_err(|_| Malformed)?;
            *expected = sequence_number.checked_add(len).ok_or(Malformed)?;
            self.agent_runs.push(AgentRun { first_event: events, len, agent, sequence_number });
            events = events.checked_add(len).filter(|_| len > 0).ok_or(Malformed)?;
        }

        let base = u32::try_from(self.content.len()).map_err(|_| Malformed)?;
        u32::try_from(self.content.len() + text.len()).map_err(|_| Malformed)?;
        self.content.push_str(&text);
        let (mut heads, mut places, mut wides) = (columns.column()?, columns.column()?, columns.column()?);
        let ops = heads.count()?;
        self.ops.reserve(ops);
        let (mut event, mut at, mut cursor) = (start, 0usize, 0u32);
        for _ in 0..ops {
            let head = heads.varint()?;
            let len = u32::try_from(head >> 4).ok().filter(|&len| len > 0).ok_or(Malformed)?;
            let pos = u32::try_from(i64::from(cursor) + unzigzag(places.varint()?)).map_err(|_| Malformed)?;
            let (kind, forward) = (if head & 1 == 1 { OpKind::Del } else { OpKind::Ins }, head & 2 == 0);
            cursor = after(kind, pos, len);
            // Only a delete right after an insert, or after another such delete, can trim it.
            let trims = head & 8 != 0;
            let follows = |before: &Op| before.first_event + before.len == event && (before.kind == OpKind::Ins || before.trims);
            if trims && (kind == OpKind::Ins || !self.ops.last().is_some_and(follows)) {
                return Err(Malformed);
            }
            let bytes = match kind {
                OpKind::Del => 0,
                OpKind::Ins if head & 4 == 0 => len as usize,
                OpKind::Ins => wides.number()? as usize,
            };
            let inserted = at.checked_add(bytes).and_then(|end| text.get(at..end)).ok_or(Malformed)?;
            match kind {
                // A character is every byte that does not carry on from the one before.
                OpKind::Ins if forward && inserted.bytes().filter(|&byte| byte as i8 >= -0x40).count() == len as usize => self.totals.0 += len,
                OpKind::Ins => return Err(Malformed),
                OpKind::Del => self.totals.1 += len,
            }
            self.ops.push(Op { first_event: event, len, kind, forward, trims, pos, content: (base + at as u32, base + (at + bytes) as u32) });
            at += bytes;
            event = event.checked_add(len).ok_or(Malformed)?;
        }

        let (mut chains, mut counts, mut backs) = (columns.column()?, Runs::of(columns.column()?), columns.column()?);
        let (mut parents, mut heads) = (Vec::new(), None);
        let mut first = start;
        for _ in 0..chains.count()? {
            let len = chains.number()?;
            parents.clear();
            match counts.next()? {
                0 => parents.push(first.checked_sub(1).ok_or(Malformed)?),
                count => {
                    for _ in 1..count {
                        parents.push(first.checked_sub(backs.number()?).filter(|&parent| parent < first).ok_or(Malformed)?);
                    }
                }
            }
            first.checked_add(len).filter(|&end| len > 0 && end <= events).ok_or(Malformed)?;
            self.graph.push(&parents, len);
            heads.get_or_insert_with(|| parents.clone());
            first += len;
        }

        let whole = file.0.is_empty() && columns.0.is_empty() && event == events && first == events && at == text.len();
        heads.filter(|_| whole).ok_or(Malformed)
    }
}

/// Returns where an op leaves offset: after what it inserted, or where it deleted from. The
/// next op is most often right there or a little before.
fn after(kind: OpKind, pos: u32, len: u32) -> u32 {
    match kind {
        OpKind::Ins => pos.saturating_add(len),
        OpKind::Del => pos,
    }
}

/// Appends a number.
fn number(out: &mut Vec<u8>, mut n: usize) {
    while n >= 0x80 {
        out.push(n as u8 | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
}

/// Appends a column: the length in bytes of what `write` writes, then that.
fn column(out: &mut Vec<u8>, write: impl FnOnce(&mut Vec<u8>)) {
    let mut values = Vec::new();
    write(&mut values);
    number(out, values.len());
    out.extend_from_slice(&values);
}

/// Appends values as runs: each value, then how many times it comes in a row.
fn runs(out: &mut Vec<u8>, values: impl Iterator<Item = u64>) {
    let mut run: Option<(u64, usize)> = None;
    for value in values {
        match &mut run {
            Some((last, times)) if *last == value => *times += 1,
            _ => {
                if let Some((last, times)) = run.replace((value, 1)) {
                    number(out, last as usize);
                    number(out, times);
                }
            }
        }
    }
    if let Some((last, times)) = run {
        number(out, last as usize);
        number(out, times);
    }
}

/// Folds a signed number into an unsigned one that is small when the number is near zero.
fn zigzag(n: i64) -> u64 {
    (n << 1 ^ n >> 63) as u64
}

fn unzigzag(n: u64) -> i64 {
    (n >> 1) as i64 ^ -((n & 1) as i64)
}

/// Compresses bytes.
fn squeezed(bytes: &[u8]) -> Vec<u8> {
    zstd::bulk::compress(bytes, 3).expect("compressing into memory")
}

/// Unpacks `len` bytes that [`squeezed`] compressed.
fn unpacked(packed: &[u8], len: usize) -> Result<Vec<u8>, Malformed> {
    // The compressed bytes say for themselves how long they unpack to, so room is only ever
    // made for a length that both agree on.
    if zstd::zstd_safe::get_frame_content_size(packed).ok().flatten() != Some(len as u64) {
        return Err(Malformed);
    }
    zstd::bulk::decompress(packed, len).ok().filter(|part| part.len() == len).ok_or(Malformed)
}

/// What is left to read of a file or of one of its columns.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    #[inline]
    fn varint(&mut self) -> Result<u64, Malformed> {
        // Most numbers fit in a byte.
        if let Some((&byte, rest)) = self.0.split_first().filter(|(&byte, _)| byte < 0x80) {
            self.0 = rest;
            return Ok(u64::from(byte));
        }
        let mut n = 0;
        for shift in (0..64).step_by(7) {
            let (&byte, rest) = self.0.split_first().ok_or(Malformed)?;
            self.0 = rest;
            n |= u64::from(byte & 0x7f) << shift;
            if byte < 0x80 {
                return Ok(n);
            }
        }
        Err(Malformed)
    }

    #[inline]
    fn number(&mut self) -> Result<u32, Malformed> {
        u32::try_from(self.varint()?).map_err(|_| Malformed)
    }

    /// Reads how many things of at least a byte each follow.
    fn count(&mut self) -> Result<usize, Malformed> {
        let items = self.number()? as usize;
        (items <= self.0.len()).then_some(items).ok_or(Malformed)
    }

    /// Takes the next `len` bytes, which [`count`](Self::count) has found to be there.
    fn take(&mut self, len: usize) -> &'a [u8] {
        let (bytes, rest) = self.0.split_at(len);
        self.0 = rest;
        bytes
    }

    /// Reads a column that [`column`] wrote.
    fn column(&mut self) -> Result<Reader<'a>, Malformed> {
        let len = self.count()?;
        Ok(Reader(self.take(len)))
    }

    /// Reads a part that was compressed: its length, its compressed length, and the bytes.
    fn packed(&mut self) -> Result<Vec<u8>, Malformed> {
        let (len, packed) = (self.number()? as usize, self.count()?);
        unpacked(self.take(packed), len)
    }
}

/// Reads values that [`runs`] wrote.
struct Runs<'a> {
    from: Reader<'a>,
    value: u64,
    left: u64,
}

impl<'a> Runs<'a> {
    fn of(from: Reader<'a>) -> Self {
        Runs { from, value: 0, left: 0 }
    }

    #[inline]
    fn next(&mut self) -> Result<u64, Malformed> {
        if self.left == 0 {
            (self.value, self.left) = (self.from.varint()?, self.from.varint()?);
            if self.left == 0 {
                return Err(Malformed);
            }
        }
        self.left -= 1;
        Ok(self.value)
    }
}
