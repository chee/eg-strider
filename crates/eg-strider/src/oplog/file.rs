use super::{OpLog, OperationKind, OperationRun, OperationRunSlice, ReplicaRun, MAX_OP_LEN};
use crate::replay::DocumentState;
use std::borrow::Cow;
use std::ops::Range;
use strider_graph::{CausalGraph, EventIndex, EventRunIndex, HistoryRange};

const MAGIC: &[u8] = b"eg-strider\x05";

/// About how many events the last part of saved history holds. Each earlier part is twice the
/// size of the one after, so that recent history reads alone and the rest packs well. Tiny in
/// debug builds, so that tests save several.
const EVENTS_PER_PART: u32 = if cfg!(debug_assertions) { 8 } else { 1 << 15 };

/// Index of a part of saved history.
pub(crate) type HistoryPartIndex = usize;

/// A file that is not a saved document, or is damaged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Malformed;

impl std::fmt::Display for Malformed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("not a saved eg-strider document")
    }
}

impl std::error::Error for Malformed {}

/// History still as saved, in parts: the events before the first that the log holds.
pub(crate) struct UnreadHistory {
    /// The file the log was loaded from.
    bytes: Vec<u8>,
    /// Where each part is in `bytes`.
    parts: Vec<Range<usize>>,
    /// Each part's first event, then the total number of events.
    starts: Vec<EventIndex>,
    /// That history's frontier.
    pub frontier: Vec<EventIndex>,
}

impl UnreadHistory {
    /// Finds the part holding an event.
    pub fn part_holding(&self, event: EventIndex) -> HistoryPartIndex {
        self.starts.partition_point(|&start| start <= event) - 1
    }
}

impl OpLog {
    /// Writes the log as a file that [`load`](Self::load) opens without merging: a header, the
    /// merged text, then the history in parts that each start at a critical version, so that a
    /// part and those after it read alone. A part stores each field as a column of LEB128
    /// varints, run-length encoded where values repeat, with positions relative to the op
    /// before and parents as distances back. Columns and text are compressed with Zstandard,
    /// each with a checksum.
    pub fn save(&mut self) -> Vec<u8> {
        self.checkout();
        let (first_held, len) = (self.graph.first_held_event(), self.graph.len());
        // History that was never read is written as it is; the rest is cut into parts.
        let kept = self.unread_history.iter().flat_map(|unread| {
            unread.parts.iter().zip(unread.starts.windows(2)).map(|(part, ends)| (Cow::from(&unread.bytes[part.clone()]), ends[1] - ends[0]))
        });
        let bounds = self.part_bounds(first_held, len);
        let new = bounds.windows(2).map(|ends| (Cow::from(self.pack(ends[0]..ends[1])), ends[1] - ends[0]));
        let parts: Vec<(Cow<[u8]>, u32)> = kept.chain(new).collect();
        let document = self.merged();
        let text = compress_with_checksum(document.text.as_bytes());

        let mut out = MAGIC.to_vec();
        number(&mut out, self.replica_names.len());
        for (name, &next) in self.replica_names.iter().zip(&self.next_sequence_numbers) {
            number(&mut out, name.len());
            out.extend_from_slice(name.as_bytes());
            number(&mut out, next as usize);
        }
        for part in [len, document.chars] {
            number(&mut out, part as usize);
        }
        let frontier = self.graph.frontier();
        number(&mut out, frontier.len());
        frontier.iter().for_each(|&event| number(&mut out, (len - event) as usize));
        number(&mut out, document.text.len());
        number(&mut out, text.len());
        number(&mut out, parts.len());
        for (part, events) in &parts {
            number(&mut out, part.len());
            number(&mut out, *events as usize);
        }
        out.extend_from_slice(&text);
        parts.iter().for_each(|(part, _)| out.extend_from_slice(part));
        out
    }

    /// Opens a file that [`save`](Self::save) wrote, ready for more events. The history is
    /// checked only when an event made at an older version has it read; reading a damaged part
    /// panics.
    pub fn load(bytes: Vec<u8>) -> Result<OpLog, Malformed> {
        let mut file = Reader(bytes.strip_prefix(MAGIC).ok_or(Malformed)?);
        let mut log = OpLog::new();
        for _ in 0..file.count()? {
            let len = file.count()?;
            log.replica_names.push(std::str::from_utf8(file.take(len)).map_err(|_| Malformed)?.to_string());
            log.next_sequence_numbers.push(file.number()?);
        }
        let (events, chars) = (file.number()?, file.number()?);
        let frontier = (0..file.count()?).map(|_| events.checked_sub(file.number()?).ok_or(Malformed)).collect::<Result<Vec<_>, _>>()?;
        let in_order = frontier.windows(2).all(|pair| pair[0] < pair[1]) && frontier.last().map_or(events == 0, |&last| last + 1 == events);
        let (text_len, text_compressed_len) = (file.number()? as usize, file.number()? as usize);
        let (mut starts, mut lens, mut end) = (vec![0], Vec::new(), 0 as EventIndex);
        for _ in 0..file.count()? {
            lens.push(file.number()? as usize);
            end = end.checked_add(file.number()?).filter(|&next| next > end).ok_or(Malformed)?;
            starts.push(end);
        }

        // The text and the parts of history follow the header, and together are the rest of the
        // file.
        let mut at = bytes.len() - file.0.len();
        let mut section = |len: usize| {
            let end = at.checked_add(len).filter(|&end| end <= bytes.len()).ok_or(Malformed)?;
            Ok(std::mem::replace(&mut at, end)..end)
        };
        let text = section(text_compressed_len)?;
        let parts = lens.into_iter().map(&mut section).collect::<Result<Vec<_>, Malformed>>()?;
        if at != bytes.len() || !in_order || end != events {
            return Err(Malformed);
        }
        let text = String::from_utf8(decompress_checked(&bytes[text], text_len)?).map_err(|_| Malformed)?;
        if text.chars().count() != chars as usize {
            return Err(Malformed);
        }

        log.graph = CausalGraph::after(events, &frontier);
        log.base = DocumentState { events, chars, text };
        log.unread_history = (events > 0).then_some(UnreadHistory { bytes, parts, starts, frontier });
        Ok(log)
    }

    /// Reads the saved history from part `from` on, so that the log holds every event from that
    /// part's first.
    pub(crate) fn unpack_from(&mut self, from: HistoryPartIndex) -> Result<(), Malformed> {
        let Some(unread) = &self.unread_history else { return Ok(()) };
        // The parts are read into a log of their own, so that a damaged one leaves this one as it was.
        let mut read = OpLog { replica_names: self.replica_names.clone(), graph: CausalGraph::after(unread.starts[from], &[]), ..OpLog::default() };
        // The first event of a part has the frontier of the history before it for parents.
        let mut frontier = Vec::new();
        for (i, part) in unread.parts[from..].iter().enumerate() {
            let first = read.unpack(&unread.bytes[part.clone()])?;
            if i == 0 {
                frontier = first;
            }
        }

        let mut unread = self.unread_history.take().expect("the saved history was just read");
        let later = std::mem::replace(self, read);
        (self.next_sequence_numbers, self.replay_cache) = (later.next_sequence_numbers, later.replay_cache);
        (self.base, self.latest_document_state) = (later.base, later.latest_document_state);
        self.replay_cache.outdate();
        let shift = self.inserted_text.len() as u32;
        self.inserted_text.push_str(&later.inserted_text);
        self.ops.extend(later.ops.into_iter().map(|op| OperationRun { inserted_text_end: op.inserted_text_end + shift, ..op }));
        self.replica_runs.extend(later.replica_runs);
        self.totals = (self.totals.0 + later.totals.0, self.totals.1 + later.totals.1);
        let runs = later.graph.runs();
        for run in 0..runs.len() {
            let parents: Vec<EventIndex> = runs.parents(run).iter().map(|&parent| runs.start(parent + 1) - 1).collect();
            // A run of the later events with no run for a parent followed the old frontier.
            let parents = if parents.is_empty() { &unread.frontier } else { &parents };
            self.graph.push(parents, runs.events(run).len() as u32);
        }

        unread.parts.truncate(from);
        unread.starts.truncate(from + 1);
        unread.frontier = frontier;
        self.unread_history = (from > 0).then_some(unread);
        Ok(())
    }

    /// Returns the parents of run `run`'s first event: for a chain with no run before it, the
    /// saved history's frontier.
    fn parents_of(&self, run: EventRunIndex) -> Vec<EventIndex> {
        let runs = self.graph.runs();
        let parents: Vec<EventIndex> = runs.parents(run).iter().map(|&parent| runs.start(parent + 1) - 1).collect();
        match &self.unread_history {
            Some(unread) if parents.is_empty() => unread.frontier.clone(),
            _ => parents,
        }
    }

    /// Chooses where to cut the held events `from..to` into parts: at critical versions, about
    /// [`EVENTS_PER_PART`] events before the end and twice as far apart each time. Returns `from`,
    /// then each part's end.
    fn part_bounds(&self, from: EventIndex, to: EventIndex) -> Vec<EventIndex> {
        let runs = self.graph.runs();
        let (mut bounds, mut size) = (vec![to], EVENTS_PER_PART);
        for range in self.graph.split_at_critical_versions().iter().rev() {
            let run_range = range.runs();
            let (start, end) = (runs.start(run_range.start).max(from), runs.start(run_range.end).min(to));
            if start >= end {
                continue;
            }
            // Anywhere in a linear range is a critical version; a concurrent range has to stay
            // whole.
            let mut last = *bounds.last().expect("the last bound");
            while matches!(range, HistoryRange::Linear(_)) && last - start > size {
                last -= size;
                bounds.push(last);
                size *= 2;
            }
            if last - start >= size {
                bounds.push(start);
                size *= 2;
            }
        }
        if *bounds.last().expect("the last bound") > from {
            bounds.push(from);
        }
        bounds.reverse();
        bounds
    }

    /// Packs the held events `events` as a part.
    fn pack(&self, events: Range<EventIndex>) -> Vec<u8> {
        let mut columns = Vec::new();

        // The runs of events by one replica. A replica's runs nearly always carry on from one
        // another, so after its first in the part a run's sequence number is stored as how
        // far it is from that.
        let first = self.replica_runs.partition_point(|run| run.first_event + run.len <= events.start);
        let replica_runs = || {
            self.replica_runs[first..].iter().take_while(|run| run.first_event < events.end).map(|run| {
                let (start, end) = (run.first_event.max(events.start), (run.first_event + run.len).min(events.end));
                (run.replica, run.sequence_number + (start - run.first_event), end - start)
            })
        };
        column(&mut columns, |out| {
            number(out, replica_runs().count());
            replica_runs().for_each(|(replica, ..)| number(out, replica as usize));
        });
        let mut next = vec![0; self.replica_names.len()];
        column(&mut columns, |out| {
            runs(
                out,
                replica_runs().map(|(replica, sequence_number, len)| {
                    zigzag(i64::from(sequence_number) - i64::from(std::mem::replace(&mut next[replica as usize], sequence_number + len)))
                }),
            )
        });
        column(&mut columns, |out| replica_runs().for_each(|(.., len)| number(out, len as usize)));

        // The ops: `len << 4 | trims << 3 | wide << 2 | backspace << 1 | delete`, the position,
        // and for a wide insert (one with characters longer than a byte) the length of its
        // text in bytes.
        let ops = || self.ops(self.op_containing(events.start)..self.op_containing(events.end), events.clone());
        let bytes = |op: &OperationRunSlice| (op.text.1 - op.text.0) as usize;
        let wide = |op: &OperationRunSlice| op.kind == OperationKind::Insert && bytes(op) != op.len as usize;
        column(&mut columns, |out| {
            number(out, ops().count());
            // A delete only trims an insert that is in the part with it.
            let mut open = false;
            for op in ops() {
                let trims = op.deletes_end_of_previous_insert && open;
                open = op.kind == OperationKind::Insert || trims;
                let flags =
                    usize::from(trims) << 3 | usize::from(wide(&op)) << 2 | usize::from(!op.forward) << 1 | usize::from(op.kind == OperationKind::Delete);
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
        // how far back they are. A chain cut off from its start by the part follows the event
        // before it, which is stored as one parent fewer than none.
        let graph_runs = self.graph.runs();
        let chains = || (graph_runs.containing(events.start)..graph_runs.len()).take_while(|&run| graph_runs.start(run) < events.end);
        let whole = |run: &EventRunIndex| graph_runs.start(*run) >= events.start;
        column(&mut columns, |out| {
            number(out, chains().count());
            for run in chains() {
                number(out, (graph_runs.start(run + 1).min(events.end) - graph_runs.start(run).max(events.start)) as usize);
            }
        });
        column(&mut columns, |out| runs(out, chains().map(|run| if whole(&run) { self.parents_of(run).len() as u64 + 1 } else { 0 })));
        column(&mut columns, |out| {
            for run in chains().filter(whole) {
                self.parents_of(run).iter().for_each(|&parent| number(out, (graph_runs.start(run) - parent) as usize));
            }
        });

        let text: String = ops().map(|op| &self.inserted_text[op.text.0 as usize..op.text.1 as usize]).collect();
        let mut out = Vec::new();
        for section in [&columns[..], text.as_bytes()] {
            let compressed = compress_with_checksum(section);
            number(&mut out, section.len());
            number(&mut out, compressed.len());
            out.extend_from_slice(&compressed);
        }
        out
    }

    /// Reads a part that [`pack`](Self::pack) wrote, whose events follow those held, and adds
    /// them. Returns the first event's parents.
    fn unpack(&mut self, part: &[u8]) -> Result<Vec<EventIndex>, Malformed> {
        let mut file = Reader(part);
        let columns = file.compressed()?;
        let text = String::from_utf8(file.compressed()?).map_err(|_| Malformed)?;
        let mut columns = Reader(&columns);
        let start = self.graph.len();

        let (mut replicas, mut ahead, mut lens) = (columns.column()?, Runs::of(columns.column()?), columns.column()?);
        let replica_runs = replicas.count()?;
        self.replica_runs.reserve(replica_runs);
        let mut next = vec![0u32; self.replica_names.len()];
        let mut events = start;
        for _ in 0..replica_runs {
            let (replica, len) = (replicas.number()?, lens.number()?);
            let expected = next.get_mut(replica as usize).ok_or(Malformed)?;
            let sequence_number = u32::try_from(i64::from(*expected) + unzigzag(ahead.next()?)).map_err(|_| Malformed)?;
            *expected = sequence_number.checked_add(len).ok_or(Malformed)?;
            self.replica_runs.push(ReplicaRun { first_event: events, len, replica, sequence_number });
            events = events.checked_add(len).filter(|_| len > 0).ok_or(Malformed)?;
        }

        let base = u32::try_from(self.inserted_text.len()).map_err(|_| Malformed)?;
        u32::try_from(self.inserted_text.len() + text.len()).map_err(|_| Malformed)?;
        self.inserted_text.push_str(&text);
        let (mut op_headers, mut places, mut wides) = (columns.column()?, columns.column()?, columns.column()?);
        let ops = op_headers.count()?;
        self.ops.reserve(ops);
        let (mut event, mut at, mut cursor) = (start, 0usize, 0u32);
        for _ in 0..ops {
            let header = op_headers.varint()?;
            let len = u32::try_from(header >> 4).ok().filter(|&len| len > 0 && len <= MAX_OP_LEN).ok_or(Malformed)?;
            let pos = u32::try_from(i64::from(cursor) + unzigzag(places.varint()?)).map_err(|_| Malformed)?;
            let (kind, forward) = (if header & 1 == 1 { OperationKind::Delete } else { OperationKind::Insert }, header & 2 == 0);
            cursor = after(kind, pos, len);
            // Only a delete right after an insert, or after another such delete, can trim it.
            let trims = header & 8 != 0;
            let follows = |before: &OperationRun| {
                before.first_event + before.len() == event && (before.kind() == OperationKind::Insert || before.deletes_end_of_previous_insert())
            };
            if trims && (kind == OperationKind::Insert || !self.ops.last().is_some_and(follows)) {
                return Err(Malformed);
            }
            let bytes = match kind {
                OperationKind::Delete => 0,
                OperationKind::Insert if header & 4 == 0 => len as usize,
                OperationKind::Insert => wides.number()? as usize,
            };
            let inserted = at.checked_add(bytes).and_then(|end| text.get(at..end)).ok_or(Malformed)?;
            match kind {
                // A character is every byte that does not carry on from the one before.
                OperationKind::Insert if forward && inserted.bytes().filter(|&byte| byte as i8 >= -0x40).count() == len as usize => self.totals.0 += len,
                OperationKind::Insert => return Err(Malformed),
                OperationKind::Delete => self.totals.1 += len,
            }
            self.ops.push(OperationRun::new(event, len, kind, forward, trims, pos, base + (at + bytes) as u32));
            at += bytes;
            event = event.checked_add(len).ok_or(Malformed)?;
        }

        let (mut chains, mut counts, mut backs) = (columns.column()?, Runs::of(columns.column()?), columns.column()?);
        let (mut parents, mut frontier_before) = (Vec::new(), None);
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
                    if !parents.is_sorted_by(|a, b| a < b) {
                        return Err(Malformed);
                    }
                }
            }
            first.checked_add(len).filter(|&end| len > 0 && end <= events).ok_or(Malformed)?;
            self.graph.push(&parents, len);
            frontier_before.get_or_insert_with(|| parents.clone());
            first += len;
        }

        let whole = file.0.is_empty() && columns.0.is_empty() && event == events && first == events && at == text.len();
        frontier_before.filter(|_| whole).ok_or(Malformed)
    }
}

/// Returns where an op leaves off: after what it inserted, or where it deleted. The next op is
/// usually there or just before.
fn after(kind: OperationKind, pos: u32, len: u32) -> u32 {
    match kind {
        OperationKind::Insert => pos.saturating_add(len),
        OperationKind::Delete => pos,
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

/// Appends a column: its length in bytes, then what `write` writes.
fn column(out: &mut Vec<u8>, write: impl FnOnce(&mut Vec<u8>)) {
    let mut values = Vec::new();
    write(&mut values);
    number(out, values.len());
    out.extend_from_slice(&values);
}

/// Appends values as runs: each value, then how many times it repeats.
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

/// Folds a signed number into an unsigned one that is small when near zero.
fn zigzag(n: i64) -> u64 {
    (n << 1 ^ n >> 63) as u64
}

fn unzigzag(n: u64) -> i64 {
    (n >> 1) as i64 ^ -((n & 1) as i64)
}

/// Compresses bytes, with a checksum that decompressing checks, so that damage is found when a
/// part is read rather than when what it holds is used.
fn compress_with_checksum(bytes: &[u8]) -> Vec<u8> {
    let mut compressor = zstd::bulk::Compressor::new(3).expect("a compression level zstd has");
    compressor.include_checksum(true).expect("zstd takes a checksum");
    compressor.compress(bytes).expect("compressing into memory")
}

/// Decompresses `len` bytes that [`compress_with_checksum`] compressed.
fn decompress_checked(compressed: &[u8], len: usize) -> Result<Vec<u8>, Malformed> {
    // The compressed bytes say for themselves how long they decompress to, so room is only ever
    // made for a length that both agree on.
    if zstd::zstd_safe::get_frame_content_size(compressed).ok().flatten() != Some(len as u64) {
        return Err(Malformed);
    }
    zstd::bulk::decompress(compressed, len).ok().filter(|bytes| bytes.len() == len).ok_or(Malformed)
}

/// What is left to read of a file or a column.
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

    /// Reads a count of things at least a byte each.
    fn count(&mut self) -> Result<usize, Malformed> {
        let items = self.number()? as usize;
        (items <= self.0.len()).then_some(items).ok_or(Malformed)
    }

    /// Takes the next `len` bytes, which [`count`](Self::count) has checked are there.
    fn take(&mut self, len: usize) -> &'a [u8] {
        let (bytes, rest) = self.0.split_at(len);
        self.0 = rest;
        bytes
    }

    /// Reads a column that [`column`](fn@column) wrote.
    fn column(&mut self) -> Result<Reader<'a>, Malformed> {
        let len = self.count()?;
        Ok(Reader(self.take(len)))
    }

    /// Reads a compressed section: its length, its compressed length, then the bytes.
    fn compressed(&mut self) -> Result<Vec<u8>, Malformed> {
        let (len, compressed_len) = (self.number()? as usize, self.count()?);
        decompress_checked(self.take(compressed_len), len)
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
