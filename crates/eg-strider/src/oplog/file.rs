//! A saved document: the log, the text it merges to, and what the graph has worked out about
//! the history, so that loading has neither to plan nor to merge. It is laid out in columns,
//! one for each field, so that a field's values sit together and pack well:
//!
//! - a number is a LEB128 varint, and a column is its length in bytes and then its values;
//! - a column whose values mostly repeat is stored as runs: a value, then how many times;
//! - an ascending list is stored as the steps between its items, and an op's position as
//!   how far it is from where the op before left off;
//! - the columns together, the inserted text and the document's text are each compressed
//!   with LZ4.

use super::{AgentRun, Op, OpKind, OpLog};
use strider_graph::{Graph, Parts};

const MAGIC: &[u8] = b"eg-strider\x02";

/// A file that is not a saved document, or is a damaged one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Malformed;

impl std::fmt::Display for Malformed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("not a saved eg-strider document")
    }
}

impl std::error::Error for Malformed {}

impl OpLog {
    /// Writes out the log with the document it merges to, as a file that [`load`](Self::load)
    /// reads back without merging anything.
    pub fn save(&mut self) -> Vec<u8> {
        let text = self.checkout();
        let mut columns = Vec::new();
        column(&mut columns, |out| {
            number(out, self.agent_names.len());
            for name in &self.agent_names {
                number(out, name.len());
                out.extend_from_slice(name.as_bytes());
            }
        });

        // The runs of events by one agent. An agent's runs nearly always carry on from one
        // another, so a run's sequence number is stored as how far it is from that.
        let mut next = vec![0; self.agent_names.len()];
        column(&mut columns, |out| {
            number(out, self.agent_runs.len());
            self.agent_runs.iter().for_each(|run| number(out, run.agent as usize));
        });
        column(&mut columns, |out| {
            let ahead = self.agent_runs.iter().map(|run| {
                let expected = std::mem::replace(&mut next[run.agent as usize], run.seq + run.len);
                zigzag(i64::from(run.seq) - i64::from(expected))
            });
            runs(out, ahead);
        });
        column(&mut columns, |out| self.agent_runs.iter().for_each(|run| number(out, run.len as usize)));

        // The ops: `len << 3 | wide << 2 | backspace << 1 | delete`, the position, and for a
        // wide insert (one with characters longer than a byte) the length of its text in bytes.
        let bytes = |op: &Op| (op.content.1 - op.content.0) as usize;
        let wide = |op: &Op| op.kind == OpKind::Ins && bytes(op) != op.len as usize;
        column(&mut columns, |out| {
            number(out, self.ops.len());
            for op in &self.ops {
                let flags = usize::from(wide(op)) << 2 | usize::from(!op.fwd) << 1 | usize::from(op.kind == OpKind::Del);
                number(out, (op.len as usize) << 3 | flags);
            }
        });
        column(&mut columns, |out| {
            let mut cursor = 0;
            for op in &self.ops {
                number(out, zigzag(i64::from(op.pos) - i64::from(std::mem::replace(&mut cursor, after(op.kind, op.pos, op.len)))) as usize);
            }
        });
        column(&mut columns, |out| self.ops.iter().filter(|op| wide(op)).for_each(|op| number(out, bytes(op))));

        // The graph's arrays, as `strider_graph::Parts` names them. A parent is stored as how
        // many blocks back it is, and what a block has seen as how far that is short of its
        // first event.
        let graph = self.graph.parts();
        let blocks = graph.seen.len();
        column(&mut columns, |out| steps(out, graph.start));
        column(&mut columns, |out| runs(out, graph.parent_start.windows(2).map(|pair| u64::from(pair[1] - pair[0]))));
        column(&mut columns, |out| {
            for block in 0..blocks {
                for parent in &graph.parents[graph.parent_start[block] as usize..graph.parent_start[block + 1] as usize] {
                    number(out, block - *parent as usize);
                }
            }
        });
        column(&mut columns, |out| steps(out, graph.heads));
        column(&mut columns, |out| runs(out, graph.seen.iter().zip(graph.start).map(|(seen, start)| u64::from(start - seen))));
        column(&mut columns, |out| steps(out, graph.critical));

        let mut out = MAGIC.to_vec();
        for part in [&columns[..], self.content.as_bytes(), text.as_bytes()] {
            let packed = lz4_flex::block::compress(part);
            number(&mut out, part.len());
            number(&mut out, packed.len());
            out.extend_from_slice(&packed);
        }
        out
    }

    /// Reads a file that [`save`](Self::save) wrote: the log, ready for more events, and its
    /// document. What the file says the graph had worked out is taken on trust.
    pub fn load(bytes: &[u8]) -> Result<(OpLog, String), Malformed> {
        let mut file = Reader(bytes.strip_prefix(MAGIC).ok_or(Malformed)?);
        let columns = file.packed()?;
        let mut log = OpLog { content: String::from_utf8(file.packed()?).map_err(|_| Malformed)?, ..OpLog::new() };
        let text = String::from_utf8(file.packed()?).map_err(|_| Malformed)?;
        u32::try_from(log.content.len()).map_err(|_| Malformed)?;
        let mut columns = Reader(&columns);

        let mut names = columns.column()?;
        for _ in 0..names.count()? {
            let len = names.count()?;
            log.agent_names.push(std::str::from_utf8(names.take(len)).map_err(|_| Malformed)?.to_string());
        }
        log.next_seq = vec![0; log.agent_names.len()];

        let (mut agents, mut ahead, mut lens) = (columns.column()?, Runs::of(columns.column()?), columns.column()?);
        let runs = agents.count()?;
        log.agent_runs.reserve_exact(runs);
        let mut events = 0u32;
        for _ in 0..runs {
            let (agent, len) = (agents.number()?, lens.number()?);
            let next = log.next_seq.get_mut(agent as usize).ok_or(Malformed)?;
            let seq = u32::try_from(i64::from(*next) + unzigzag(ahead.next()?)).map_err(|_| Malformed)?;
            *next = seq.checked_add(len).ok_or(Malformed)?;
            log.agent_runs.push(AgentRun { lv: events, len, agent, seq });
            events = events.checked_add(len).filter(|_| len > 0).ok_or(Malformed)?;
        }

        let (mut heads, mut places, mut wides) = (columns.column()?, columns.column()?, columns.column()?);
        let ops = heads.count()?;
        log.ops.reserve_exact(ops);
        let (mut lv, mut at, mut cursor) = (0u32, 0usize, 0u32);
        for _ in 0..ops {
            let head = heads.varint()?;
            let len = u32::try_from(head >> 3).ok().filter(|&len| len > 0).ok_or(Malformed)?;
            let pos = u32::try_from(i64::from(cursor) + unzigzag(places.varint()?)).map_err(|_| Malformed)?;
            let (kind, fwd) = (if head & 1 == 1 { OpKind::Del } else { OpKind::Ins }, head & 2 == 0);
            cursor = after(kind, pos, len);
            let bytes = match kind {
                OpKind::Del => 0,
                OpKind::Ins if head & 4 == 0 => len as usize,
                OpKind::Ins => wides.number()? as usize,
            };
            let inserted = at.checked_add(bytes).and_then(|end| log.content.get(at..end)).ok_or(Malformed)?;
            match kind {
                // A character is every byte that does not carry on from the one before.
                OpKind::Ins if fwd && inserted.bytes().filter(|&byte| byte as i8 >= -0x40).count() == len as usize => log.totals.0 += len,
                OpKind::Ins => return Err(Malformed),
                OpKind::Del => log.totals.1 += len,
            }
            log.ops.push(Op { lv, len, kind, fwd, pos, content: (at as u32, (at + bytes) as u32) });
            at += bytes;
            lv = lv.checked_add(len).ok_or(Malformed)?;
        }

        let start = columns.column()?.steps()?;
        let blocks = start.len().checked_sub(1).ok_or(Malformed)?;
        let mut counts = Runs::of(columns.column()?);
        let mut parent_start = Vec::with_capacity(start.len());
        let mut total = 0u32;
        parent_start.push(total);
        for _ in 0..blocks {
            total = u32::try_from(counts.next()?).ok().and_then(|count| total.checked_add(count)).ok_or(Malformed)?;
            parent_start.push(total);
        }
        let mut backs = columns.column()?;
        let mut parents = Vec::with_capacity(backs.room(total as usize)?);
        for block in 0..blocks {
            for _ in parent_start[block]..parent_start[block + 1] {
                parents.push((block as u32).checked_sub(backs.number()?).ok_or(Malformed)?);
            }
        }
        let heads = columns.column()?.steps()?;
        let mut short = Runs::of(columns.column()?);
        let seen = start[..blocks].iter().map(|&start| u32::try_from(short.next()?).ok().and_then(|short| start.checked_sub(short)).ok_or(Malformed)).collect::<Result<_, _>>()?;
        let critical = columns.column()?.steps()?;
        log.graph = Graph::from_parts(Parts { start, parent_start, parents, heads, seen, critical }).ok_or(Malformed)?;

        let whole = file.0.is_empty() && columns.0.is_empty() && lv == events && at == log.content.len() && log.graph.len() == events;
        whole.then_some((log, text)).ok_or(Malformed)
    }
}

/// Returns where an op leaves off: after what it inserted, or where it deleted from. The
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

/// Appends an ascending list as its length, its first item, and then each step up.
fn steps(out: &mut Vec<u8>, list: &[u32]) {
    number(out, list.len());
    let mut last = 0;
    for &item in list {
        number(out, (item - std::mem::replace(&mut last, item)) as usize);
    }
}

/// Folds a signed number into an unsigned one that is small when the number is near zero.
fn zigzag(n: i64) -> u64 {
    (n << 1 ^ n >> 63) as u64
}

fn unzigzag(n: u64) -> i64 {
    (n >> 1) as i64 ^ -((n & 1) as i64)
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

    /// Checks that `items` things of at least a byte each can still follow.
    fn room(&self, items: usize) -> Result<usize, Malformed> {
        (items <= self.0.len()).then_some(items).ok_or(Malformed)
    }

    /// Reads how many things of at least a byte each follow.
    fn count(&mut self) -> Result<usize, Malformed> {
        let items = self.number()? as usize;
        self.room(items)
    }

    /// Takes the next `len` bytes, which [`room`](Self::room) has found to be there.
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

    /// Reads a list that [`steps`] wrote.
    fn steps(mut self) -> Result<Vec<u32>, Malformed> {
        let mut last = 0u32;
        (0..self.count()?)
            .map(|_| {
                last = last.checked_add(self.number()?).ok_or(Malformed)?;
                Ok(last)
            })
            .collect()
    }

    /// Reads a part that was compressed: its length, its compressed length, and the bytes.
    fn packed(&mut self) -> Result<Vec<u8>, Malformed> {
        let (len, packed) = (self.number()? as usize, self.count()?);
        // LZ4 cannot pack anything to less than a 255th of its size.
        let bytes = self.take(packed);
        if len > packed.saturating_mul(255) {
            return Err(Malformed);
        }
        lz4_flex::block::decompress(bytes, len).ok().filter(|part| part.len() == len).ok_or(Malformed)
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
