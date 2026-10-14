mod file;

pub use file::Malformed;

use crate::replay::{self, Base, Memo};
use std::cmp::Ordering;
use std::ops::Range;
use strider_graph::{EventId, Graph};

/// How many events an agent made before this one. With the agent, it names an event on every
/// replica.
pub type SequenceNumber = u32;
pub type AgentId = u32;

/// Index of an op in the log.
pub(crate) type OpIndex = u32;

/// Where some inserted text is in [`OpLog::content`]: its first byte, and the byte after its
/// last.
pub(crate) type ByteRange = (u32, u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OpKind {
    Ins,
    Del,
}

/// One op of a run handed to [`OpLog::add_remote_ops`]. Positions and lengths count
/// characters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edit {
    /// Inserts the next `len` characters of the run's text at `pos`.
    Insert { pos: u32, len: u32 },
    /// Deletes the `len` characters at `pos`, the last of them first if `backspace`.
    Delete { pos: u32, len: u32, backspace: bool },
}

/// A run of consecutive events of one kind.
struct Op {
    first_event: EventId,
    len: u32,
    kind: OpKind,
    /// Deletes only: false for backspace, whose first event removes the last position.
    forward: bool,
    /// Deletes only: removes the end of what the insert before it put in. Found when a run of
    /// ops arrives together.
    trims: bool,
    /// The positions `pos..pos + len` that the run touches, before its first event.
    pos: u32,
    /// Inserts only: the inserted text.
    content: ByteRange,
}

/// A run of consecutive events by one agent.
struct AgentRun {
    first_event: EventId,
    len: u32,
    agent: AgentId,
    sequence_number: SequenceNumber,
}

/// Part of an op, as the replay sees it.
#[derive(Clone, Copy)]
pub(crate) struct OpSlice {
    pub first_event: EventId,
    pub len: u32,
    pub kind: OpKind,
    pub forward: bool,
    /// Whether this is a whole delete that trims the insert before it.
    pub trims: bool,
    /// Inserts go at `pos`; deletes remove `pos..pos + len`.
    pub pos: u32,
    /// Inserts only: the inserted text.
    pub text: ByteRange,
}

/// The operation log: every insert and delete this replica knows of, in the order it learned of
/// them, with each event's parents, author and position. Consecutive events are run-length
/// encoded into ops, and inserted text lives in one append-only buffer. A checkout derives the
/// document in the style of Eg-walker (Gentle and Kleppmann, 2024), building CRDT state only
/// where history is concurrent; on top of that, regions whose concurrent edits do not touch are
/// merged by shifting positions, what the CRDT works out is memoised at critical versions, and
/// the text is built backwards without a rope.
#[derive(Default)]
pub struct OpLog {
    pub(crate) graph: Graph,
    ops: Vec<Op>,
    content: String,
    agent_names: Vec<String>,
    next_sequence_numbers: Vec<SequenceNumber>,
    agent_runs: Vec<AgentRun>,
    memo: Memo,
    /// The document as of some first events, which the next merge starts from.
    base: Base,
    /// The fully merged document, when the log ends with concurrent events. The base stays put
    /// then, since moving it into events still being added would cut them up at every merge.
    latest: Option<Base>,
    /// History from before the first event held here, from a loaded file, still unread.
    packed: Option<file::Packed>,
    /// Scratch for a new event's parents.
    parents: Vec<EventId>,
    /// The characters the inserts put in, and the characters the deletes take out.
    totals: (u32, u32),
}

/// Returns how many bytes of `text` come before the first that is not ASCII.
fn ascii_prefix(text: &[u8]) -> usize {
    // Whole blocks are checked a word at a time; only the block that fails is searched.
    let blocks = text.chunks(256).take_while(|block| block.is_ascii()).count();
    let rest = &text[(blocks * 256).min(text.len())..];
    text.len() - rest.len() + rest.iter().take_while(|byte| byte.is_ascii()).count()
}

impl OpLog {
    /// Creates an empty log.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the number of events: characters inserted plus characters deleted.
    pub fn len(&self) -> usize {
        self.graph.len() as usize
    }

    /// Tells whether the log has no events.
    pub fn is_empty(&self) -> bool {
        self.graph.is_empty()
    }

    /// Returns the id of the agent called `name`, registering it if it is new.
    pub fn get_or_create_agent(&mut self, name: &str) -> AgentId {
        if let Some(i) = self.agent_names.iter().position(|n| n == name) {
            return i as AgentId;
        }
        self.agent_names.push(name.to_string());
        self.next_sequence_numbers.push(0);
        (self.agent_names.len() - 1) as AgentId
    }

    /// Returns an agent's name.
    pub fn agent_name(&self, agent: AgentId) -> &str {
        &self.agent_names[agent as usize]
    }

    /// Returns the events with no children: the version a local edit builds on.
    pub fn frontier(&self) -> Vec<EventId> {
        self.graph.frontier().to_vec()
    }

    /// Merges every event and returns the document, going through only the events since the
    /// last merge where history allows.
    pub fn checkout(&mut self) -> &str {
        let len = self.graph.len();
        if self.base.events != len && self.latest.as_ref().is_none_or(|latest| latest.events != len) {
            // An event made without all that the base holds is concurrent with some of it.
            // The merge then has to tell what the base holds from the rest.
            if self.graph.cut(self.base.events) {
                self.split_op(self.base.events);
                self.memo.outdate();
            }
            let mut memo = std::mem::take(&mut self.memo);
            let (text, chars) = replay::checkout(self, &mut memo, &self.base);
            self.memo = memo;
            let merged = Base { events: len, chars, text };
            if self.graph.frontier().len() == 1 {
                (self.base, self.latest) = (merged, None);
            } else {
                self.latest = Some(merged);
            }
        }
        &self.merged().text
    }

    /// Returns the document as of the last merge.
    fn merged(&self) -> &Base {
        self.latest.as_ref().unwrap_or(&self.base)
    }

    /// Returns how many events of a loaded log's history are still unread.
    pub fn unread(&self) -> usize {
        self.graph.floor() as usize
    }

    /// Returns how many regions the last checkout could not merge by shifting alone.
    pub fn tracked_regions(&self) -> usize {
        self.memo.tracked_regions()
    }

    /// Drops what earlier checkouts worked out, so that the next one starts from scratch.
    pub fn forget(&mut self) {
        self.unpack_from(0);
        (self.memo, self.base, self.latest) = (Memo::default(), Base::default(), None);
    }

    /// Inserts `text` at `pos` in the document as of the current frontier.
    pub fn add_insert(&mut self, agent: AgentId, pos: usize, text: &str) -> Range<EventId> {
        self.push(agent, None, None, OpKind::Ins, true, pos, text.chars().count(), text)
    }

    /// Deletes `range` from the document as of the current frontier.
    pub fn add_delete(&mut self, agent: AgentId, range: Range<usize>) -> Range<EventId> {
        self.push(agent, None, None, OpKind::Del, true, range.start, range.len(), "")
    }

    /// Inserts `text` at `pos` in the document as of version `parents`.
    pub fn add_insert_at(&mut self, agent: AgentId, parents: &[EventId], pos: usize, text: &str) -> Range<EventId> {
        self.push(agent, None, Some(parents), OpKind::Ins, true, pos, text.chars().count(), text)
    }

    /// Deletes `range` from the document as of version `parents`. `forward` is false for
    /// backspace, which removes the last character first.
    pub fn add_delete_at(&mut self, agent: AgentId, parents: &[EventId], range: Range<usize>, forward: bool) -> Range<EventId> {
        self.push(agent, None, Some(parents), OpKind::Del, forward, range.start, range.len(), "")
    }

    /// As [`add_insert_at`](Self::add_insert_at), for an event from elsewhere with a fixed
    /// sequence number.
    pub fn add_remote_insert(&mut self, agent: AgentId, sequence_number: SequenceNumber, parents: &[EventId], pos: usize, text: &str) -> Range<EventId> {
        self.push(agent, Some(sequence_number), Some(parents), OpKind::Ins, true, pos, text.chars().count(), text)
    }

    /// As [`add_delete_at`](Self::add_delete_at), for an event from elsewhere with a fixed
    /// sequence number.
    pub fn add_remote_delete(
        &mut self,
        agent: AgentId,
        sequence_number: SequenceNumber,
        parents: &[EventId],
        range: Range<usize>,
        forward: bool,
    ) -> Range<EventId> {
        self.push(agent, Some(sequence_number), Some(parents), OpKind::Del, forward, range.start, range.len(), "")
    }

    /// Appends a run of ops by one agent from elsewhere: the first made at `parents`, each
    /// other following the one before. `text` holds the inserts' text, in order.
    pub fn add_remote_ops(&mut self, agent: AgentId, sequence_number: SequenceNumber, parents: &[EventId], ops: &[Edit], text: &str) -> Range<EventId> {
        let parts = |edit: &Edit| match *edit {
            Edit::Insert { pos, len } => (OpKind::Ins, true, pos, len),
            Edit::Delete { pos, len, backspace } => (OpKind::Del, !backspace, pos, len),
        };
        let event = self.graph.len();
        let total = ops.iter().try_fold(0u32, |total, edit| total.checked_add(parts(edit).3)).expect("run too long");
        if total == 0 {
            return event..event;
        }
        let (mut follows, events) = self.link(Some(parents), total);
        self.assign(agent, Some(sequence_number), event, total);

        let mut at = self.content.len() as u32;
        self.content.push_str(text);
        let end = u32::try_from(self.content.len()).expect("too much inserted text");
        // Characters and bytes coincide in ASCII text. `plain` is where the text stops being
        // ASCII: found again only once an insert reaches it, since such characters are few.
        let ascii_until = |from: u32| from + ascii_prefix(&text.as_bytes()[(from - (end - text.len() as u32)) as usize..]) as u32;
        let mut plain = ascii_until(at);
        // The insert just before, if only deletes of its end have followed: where what is
        // left of it ends, and how much is left.
        let (mut next, mut open) = (event, None::<(u32, u32)>);
        for (kind, forward, pos, len) in ops.iter().map(parts).filter(|&(.., len)| len > 0) {
            let bytes = match kind {
                OpKind::Del => 0,
                OpKind::Ins if len <= plain.saturating_sub(at) => len,
                OpKind::Ins => {
                    let bytes = self.prefix((at, end), len);
                    plain = ascii_until(at + bytes);
                    bytes
                }
            };
            assert!(bytes <= end - at, "text does not match the inserts");
            let trims = kind == OpKind::Del && open.is_some_and(|(end, left)| pos + len == end && len <= left);
            let whole = self.store(follows && !trims, Op { first_event: next, len, kind, forward, trims, pos, content: (at, at + bytes) });
            open = match kind {
                OpKind::Ins => Some((pos + len, whole)),
                OpKind::Del if trims => open.map(|(end, left)| (end - len, left - len)),
                OpKind::Del => None,
            };
            (at, next, follows) = (at + bytes, next + len, true);
        }
        assert_eq!(at, end, "text does not match the inserts");
        events
    }

    /// Appends a run of events of one kind, extending the last op when it simply carries on.
    #[allow(clippy::too_many_arguments)]
    fn push(
        &mut self,
        agent: AgentId,
        sequence_number: Option<SequenceNumber>,
        parents: Option<&[EventId]>,
        kind: OpKind,
        forward: bool,
        pos: usize,
        len: usize,
        text: &str,
    ) -> Range<EventId> {
        let event = self.graph.len();
        if len == 0 {
            return event..event;
        }
        let len = u32::try_from(len).expect("run too long");
        let pos = u32::try_from(pos).expect("position too large");
        let (continues, events) = self.link(parents, len);
        self.assign(agent, sequence_number, event, len);
        let at = self.content.len() as u32;
        self.content.push_str(text);
        self.store(continues, Op { first_event: event, len, kind, forward, trims: false, pos, content: (at, self.content.len() as u32) });
        events
    }

    /// Adds a chain of `len` events to the graph, the first at `parents` or else the frontier.
    /// Returns whether they follow the last event, and their ids.
    fn link(&mut self, parents: Option<&[EventId]>, len: u32) -> (bool, Range<EventId>) {
        // History that is still as it was saved has to be read for an event that was made
        // without all of it: one with no parent held here, and not every old head for parents.
        // A block of it starts at a critical version, so an event has all that comes before
        // the block holding its latest parent.
        if let (Some(packed), Some(parents)) = (&self.packed, parents) {
            let floor = self.graph.floor();
            if !parents.iter().any(|&parent| parent >= floor) && !packed.heads.iter().all(|head| parents.contains(head)) {
                let block = parents.iter().max().map_or(0, |&parent| packed.block_holding(parent));
                self.unpack_from(block);
            }
        }
        let event = self.graph.len();
        // Most events follow the last one, whose number is then all the graph needs.
        let continues = event > 0 && parents.map_or(self.graph.frontier() == [event - 1], |parents| parents == [event - 1]);
        if continues {
            return (true, self.graph.push(&[event - 1], len));
        }
        let mut at = std::mem::take(&mut self.parents);
        at.clear();
        match parents {
            Some(parents) => at.extend_from_slice(parents),
            None => at.extend_from_slice(self.graph.frontier()),
        }
        let events = self.graph.push(&at, len);
        self.parents = at;
        (false, events)
    }

    /// Records that `agent` made the `len` events from `event`, numbered from `sequence_number`
    /// or else from where its last left off.
    fn assign(&mut self, agent: AgentId, sequence_number: Option<SequenceNumber>, event: EventId, len: u32) {
        let sequence_number = sequence_number.unwrap_or(self.next_sequence_numbers[agent as usize]);
        let next = &mut self.next_sequence_numbers[agent as usize];
        *next = (*next).max(sequence_number + len);
        match self.agent_runs.last_mut() {
            Some(r) if r.agent == agent && r.first_event + r.len == event && r.sequence_number + r.len == sequence_number => r.len += len,
            _ => self.agent_runs.push(AgentRun { first_event: event, len, agent, sequence_number }),
        }
    }

    /// Stores an op, merging it into the last op when it carries it on, as typing or holding
    /// delete does. Returns the length of the op it ends up in.
    fn store(&mut self, follows: bool, op: Op) -> u32 {
        match op.kind {
            OpKind::Ins => self.totals.0 += op.len,
            OpKind::Del => self.totals.1 += op.len,
        }
        match self.ops.last_mut() {
            Some(o)
                if follows
                    && o.kind == op.kind
                    && o.forward
                    && op.forward
                    && !o.trims
                    && ((op.kind == OpKind::Ins && o.pos + o.len == op.pos) || (op.kind == OpKind::Del && o.pos == op.pos)) =>
            {
                o.len += op.len;
                o.content.1 = op.content.1;
                o.len
            }
            _ => {
                let len = op.len;
                self.ops.push(op);
                len
            }
        }
    }

    /// Makes an op start at event `at`, if one runs across it.
    fn split_op(&mut self, at: EventId) {
        let i = self.op_containing(at) as usize;
        let Some(op) = self.ops.get(i).filter(|op| op.first_event < at) else { return };
        let end = op.first_event + op.len;
        let part = |events: Range<EventId>| {
            let part = self.slice(op, &events);
            Op { first_event: part.first_event, len: part.len, kind: part.kind, forward: part.forward, trims: false, pos: part.pos, content: part.text }
        };
        let parts = [part(op.first_event..at), part(at..end)];
        self.ops.splice(i..=i, parts);
        // A delete only trims straight after an insert or after another delete that does.
        for after in self.ops[i + 2..].iter_mut().take_while(|after| after.trims) {
            after.trims = false;
        }
    }

    /// Returns how many bytes the first `len` characters of the text at `content` take.
    #[cold]
    fn prefix(&self, content: ByteRange, len: u32) -> u32 {
        let text = self.text(content);
        match text.char_indices().nth(len as usize) {
            Some((bytes, _)) => bytes as u32,
            None => {
                assert_eq!(text.chars().count(), len as usize, "text does not match the inserts");
                text.len() as u32
            }
        }
    }

    /// Returns the text at `content` without its last `cut` characters: what a
    /// [trimming](Op::trims) delete leaves of the insert.
    pub(crate) fn trimmed(&self, content: ByteRange, len: u32, cut: u32) -> ByteRange {
        if content.1 - content.0 == len {
            return (content.0, content.1 - cut);
        }
        (content.0, content.0 + self.prefix(content, len - cut))
    }

    /// Returns the agent that made `event`, and its sequence number.
    fn agent_at(&self, event: EventId) -> (AgentId, SequenceNumber) {
        let run = &self.agent_runs[self.agent_runs.partition_point(|r| r.first_event <= event) - 1];
        (run.agent, run.sequence_number + (event - run.first_event))
    }

    /// Breaks a tie between concurrent inserts at the same place: does the insert from `a` go
    /// before the one from `b`?
    pub(crate) fn inserts_before(&self, a: EventId, b: EventId) -> bool {
        let (agent_a, sequence_number_a) = self.agent_at(a);
        let (agent_b, sequence_number_b) = self.agent_at(b);
        match self.agent_names[agent_a as usize].cmp(&self.agent_names[agent_b as usize]) {
            Ordering::Less => true,
            Ordering::Greater => false,
            Ordering::Equal => sequence_number_a < sequence_number_b,
        }
    }

    /// Returns, for each event in ascending `starts`, the index of the op holding it, or the
    /// number of ops for the event after the last.
    pub(crate) fn ops_containing(&self, starts: impl Iterator<Item = EventId>) -> Vec<OpIndex> {
        let mut op = 0;
        starts
            .map(|event| {
                while self.ops.get(op).is_some_and(|op| op.first_event + op.len <= event) {
                    op += 1;
                }
                op as OpIndex
            })
            .collect()
    }

    /// Iterates over the parts of ops inside `events`. `ops` runs from the op holding the first
    /// event to the op holding the event after the last.
    pub(crate) fn ops(&self, ops: Range<OpIndex>, events: Range<EventId>) -> impl DoubleEndedIterator<Item = OpSlice> + '_ {
        // The op containing the event after the last holds some of these too if it starts
        // before then.
        let straddles = self.ops.get(ops.end as usize).is_some_and(|op| op.first_event < events.end);
        let count = if events.is_empty() { 0 } else { (ops.end - ops.start) as usize + straddles as usize };
        self.ops[ops.start as usize..][..count].iter().map(move |op| self.slice(op, &events))
    }

    /// Returns the index of the op holding `event`.
    pub(crate) fn op_containing(&self, event: EventId) -> OpIndex {
        self.ops.partition_point(|op| op.first_event + op.len <= event) as OpIndex
    }

    /// Returns all inserted text, which [`OpSlice`] ranges point into.
    pub(crate) fn content(&self) -> &str {
        &self.content
    }

    /// Returns the inserted text at a range.
    fn text(&self, (start, end): ByteRange) -> &str {
        &self.content[start as usize..end as usize]
    }

    /// Returns what [`lengths`](Self::lengths) would count over every event.
    pub(crate) fn total_lengths(&self) -> (u32, u32) {
        self.totals
    }

    /// Counts the characters `events` insert and remove, applied as recorded. `first_op` is the
    /// op holding the first of them.
    pub(crate) fn lengths(&self, first_op: OpIndex, events: Range<EventId>) -> (u32, u32) {
        let (mut inserted, mut removed) = (0, 0);
        for op in &self.ops[first_op as usize..] {
            if op.first_event.max(events.start) >= events.end {
                break;
            }
            let len = events.end.min(op.first_event + op.len) - events.start.max(op.first_event);
            // Which kind an op is cannot be predicted, so both sums are added to without
            // branching on it.
            let insert = (op.kind == OpKind::Ins) as u32;
            inserted += len * insert;
            removed += len * (1 - insert);
        }
        (inserted, removed)
    }

    /// Returns the bytes of characters `from..to` of the text at `content`.
    #[cold]
    fn cut(&self, content: ByteRange, from: u32, to: u32) -> ByteRange {
        let text = self.text(content);
        // Where each character starts, then where the text ends.
        let mut starts = text.char_indices().map(|(byte, _)| byte as u32).chain([text.len() as u32]);
        let first = starts.nth(from as usize).expect("an op is cut within its text");
        let last = if from < to { starts.nth((to - from - 1) as usize).expect("an op is cut within its text") } else { first };
        (content.0 + first, content.0 + last)
    }

    /// Cuts out the part of `op` inside `events`.
    #[inline]
    fn slice(&self, op: &Op, events: &Range<EventId>) -> OpSlice {
        let from = events.start.max(op.first_event) - op.first_event;
        let to = events.end.min(op.first_event + op.len) - op.first_event;
        // Nearly always the whole op, which needs no working out whatever its kind.
        if to - from == op.len {
            return OpSlice { first_event: op.first_event, len: op.len, kind: op.kind, forward: op.forward, trims: op.trims, pos: op.pos, text: op.content };
        }
        let (pos, text) = match (op.kind, op.forward) {
            (OpKind::Ins, _) => {
                let (start, end) = op.content;
                // Characters and bytes coincide when the text is all ASCII.
                let text = if end - start == op.len { (start + from, start + to) } else { self.cut(op.content, from, to) };
                (op.pos + from, text)
            }
            // Each forward delete lands on the same position, so a later slice starts where
            // the op did.
            (OpKind::Del, true) => (op.pos, (0, 0)),
            (OpKind::Del, false) => (op.pos + op.len - to, (0, 0)),
        };
        OpSlice { first_event: op.first_event + from, len: to - from, kind: op.kind, forward: op.forward, trims: false, pos, text }
    }
}
