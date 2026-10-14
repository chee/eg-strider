//! The operation log: every single-character insert and delete this replica knows of, in
//! the order it learned of them, with each event's parents, author and original position.
//! This is all that is stored or sent; the document is derived from it by a checkout.
//! Consecutive events are run-length encoded into ops (typing a word is one op), and
//! inserted text lives in one append-only buffer that ops and edits point into, as in a
//! piece table.

mod file;

pub use file::Malformed;

use crate::replay::{self, Base, Memo};
use std::cmp::Ordering;
use std::ops::Range;
use strider_graph::{Event, Graph};

/// A local version: the number of an event in one replica's log. Numbers are not shared
/// between replicas; any numbering that puts parents before children is valid.
pub type LV = usize;
pub type AgentId = u32;

/// An op's place in the log's list of ops.
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

/// A run of consecutive single-character events of one kind.
struct Op {
    lv: Event,
    len: u32,
    kind: OpKind,
    /// Deletes only: false for backspace, where the first event removes the last position.
    fwd: bool,
    /// Deletes only: the op takes back the end of what the insert before it put in, or of
    /// what earlier such deletes have left of it. Found when a run of ops arrives together.
    trims: bool,
    /// The run touches document positions `pos..pos + len`, measured before its first
    /// event.
    pos: u32,
    /// Inserts only: the inserted text.
    content: ByteRange,
}

/// A consecutive run of events by one agent.
struct AgentRun {
    lv: Event,
    len: u32,
    agent: AgentId,
    seq: u32,
}

/// Part of one op, as handed to the replayer.
#[derive(Clone, Copy)]
pub(crate) struct OpSlice {
    pub lv: Event,
    pub len: u32,
    pub kind: OpKind,
    pub fwd: bool,
    /// Whether this is the whole of a delete that takes back the end of the insert before it.
    pub trims: bool,
    /// Inserts go at `pos`; deletes remove `pos..pos + len`.
    pub pos: u32,
    /// Inserts only: the inserted text.
    pub text: ByteRange,
}

#[derive(Default)]
pub struct OpLog {
    pub(crate) graph: Graph,
    ops: Vec<Op>,
    content: String,
    agent_names: Vec<String>,
    next_seq: Vec<u32>,
    agent_runs: Vec<AgentRun>,
    memo: Memo,
    /// The document as of some number of the first events, which the next merge starts from.
    base: Base,
    /// The document with every event merged, when the log ends with events that are concurrent
    /// with one another. The base is left where it is then: moving it into the middle of
    /// events still being added to would have each merge cut them up further.
    latest: Option<Base>,
    /// The history from before the first event held here, if the log was loaded and has not
    /// had to read all of it.
    packed: Option<file::Packed>,
    /// Reused to hold the parents of the event being added.
    parents: Vec<Event>,
    /// How many characters the inserts put in, and how many the deletes take out.
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

    /// Returns the number of events (inserted plus deleted characters).
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
        self.next_seq.push(0);
        (self.agent_names.len() - 1) as AgentId
    }

    /// Returns an agent's name.
    pub fn agent_name(&self, agent: AgentId) -> &str {
        &self.agent_names[agent as usize]
    }

    /// Returns the events with no children: the version a new local edit would build on.
    pub fn frontier(&self) -> Vec<LV> {
        self.graph.frontier().iter().map(|&lv| lv as LV).collect()
    }

    /// Merges every event in the log and returns the document. Only the events since the
    /// last merge are gone through, where the history allows it.
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

    /// Returns how many events of a loaded log's history are still as they were saved, unread.
    pub fn unread(&self) -> usize {
        self.graph.floor() as usize
    }

    /// Returns how many regions the last checkout could not merge by shifting alone.
    pub fn tracked_regions(&self) -> usize {
        self.memo.tracked_regions()
    }

    /// Drops everything earlier checkouts worked out. The next one starts from scratch.
    pub fn forget(&mut self) {
        self.unpack_from(0);
        (self.memo, self.base, self.latest) = (Memo::default(), Base::default(), None);
    }

    /// Inserts `text` at `pos` in the document as of the current frontier.
    pub fn add_insert(&mut self, agent: AgentId, pos: usize, text: &str) -> Range<LV> {
        self.push(agent, None, None, OpKind::Ins, true, pos, text.chars().count(), text)
    }

    /// Deletes `range` from the document as of the current frontier.
    pub fn add_delete(&mut self, agent: AgentId, range: Range<usize>) -> Range<LV> {
        self.push(agent, None, None, OpKind::Del, true, range.start, range.len(), "")
    }

    /// Inserts `text` at `pos` in the document as of version `parents`.
    pub fn add_insert_at(&mut self, agent: AgentId, parents: &[LV], pos: usize, text: &str) -> Range<LV> {
        self.push(agent, None, Some(parents), OpKind::Ins, true, pos, text.chars().count(), text)
    }

    /// Deletes `range` from the document as of version `parents`. `fwd` is false when the
    /// characters were removed last-first, as backspace does.
    pub fn add_delete_at(&mut self, agent: AgentId, parents: &[LV], range: Range<usize>, fwd: bool) -> Range<LV> {
        self.push(agent, None, Some(parents), OpKind::Del, fwd, range.start, range.len(), "")
    }

    /// Does as [`add_insert_at`](Self::add_insert_at), for an event received from elsewhere
    /// whose sequence number for `agent` is already fixed.
    pub fn add_remote_insert(&mut self, agent: AgentId, seq: usize, parents: &[LV], pos: usize, text: &str) -> Range<LV> {
        self.push(agent, Some(seq as u32), Some(parents), OpKind::Ins, true, pos, text.chars().count(), text)
    }

    /// Does as [`add_delete_at`](Self::add_delete_at), with a fixed sequence number.
    pub fn add_remote_delete(&mut self, agent: AgentId, seq: usize, parents: &[LV], range: Range<usize>, fwd: bool) -> Range<LV> {
        self.push(agent, Some(seq as u32), Some(parents), OpKind::Del, fwd, range.start, range.len(), "")
    }

    /// Appends a run of ops by one agent, received from elsewhere: the first is made at
    /// version `parents` and each of the others follows the one before. `text` holds what the
    /// inserts put in, one after another.
    pub fn add_remote_ops(&mut self, agent: AgentId, seq: usize, parents: &[LV], ops: &[Edit], text: &str) -> Range<LV> {
        let parts = |edit: &Edit| match *edit {
            Edit::Insert { pos, len } => (OpKind::Ins, true, pos, len),
            Edit::Delete { pos, len, backspace } => (OpKind::Del, !backspace, pos, len),
        };
        let lv = self.graph.len();
        let total = ops.iter().try_fold(0u32, |total, edit| total.checked_add(parts(edit).3)).expect("run too long");
        if total == 0 {
            return lv as LV..lv as LV;
        }
        let (mut follows, events) = self.link(Some(parents), total);
        self.assign(agent, Some(seq as u32), lv, total);

        let mut at = self.content.len() as u32;
        self.content.push_str(text);
        let end = u32::try_from(self.content.len()).expect("too much inserted text");
        // Characters and bytes coincide in ASCII text. `plain` is where the text stops being
        // ASCII: found again only once an insert reaches it, since such characters are few.
        let ascii_until = |from: u32| from + ascii_prefix(&text.as_bytes()[(from - (end - text.len() as u32)) as usize..]) as u32;
        let mut plain = ascii_until(at);
        // The insert just before, if only deletes of its end have followed: where what is
        // left of it ends, and how much is left.
        let (mut next, mut open) = (lv, None::<(u32, u32)>);
        for (kind, fwd, pos, len) in ops.iter().map(parts).filter(|&(.., len)| len > 0) {
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
            let whole = self.store(follows && !trims, Op { lv: next, len, kind, fwd, trims, pos, content: (at, at + bytes) });
            open = match kind {
                OpKind::Ins => Some((pos + len, whole)),
                OpKind::Del if trims => open.map(|(end, left)| (end - len, left - len)),
                OpKind::Del => None,
            };
            (at, next, follows) = (at + bytes, next + len, true);
        }
        assert_eq!(at, end, "text does not match the inserts");
        events.start as LV..events.end as LV
    }

    /// Appends a run of events of one kind, extending the last op when it simply carries on.
    #[allow(clippy::too_many_arguments)]
    fn push(&mut self, agent: AgentId, seq: Option<u32>, parents: Option<&[LV]>, kind: OpKind, fwd: bool, pos: usize, len: usize, text: &str) -> Range<LV> {
        let lv = self.graph.len();
        if len == 0 {
            return lv as LV..lv as LV;
        }
        let len = u32::try_from(len).expect("run too long");
        let pos = u32::try_from(pos).expect("position too large");
        let (continues, events) = self.link(parents, len);
        self.assign(agent, seq, lv, len);
        let at = self.content.len() as u32;
        self.content.push_str(text);
        self.store(continues, Op { lv, len, kind, fwd, trims: false, pos, content: (at, self.content.len() as u32) });
        events.start as LV..events.end as LV
    }

    /// Adds `len` events in a chain to the graph, the first at version `parents` or, without
    /// any, at the current frontier. Returns whether they simply follow the last event, and
    /// their numbers.
    fn link(&mut self, parents: Option<&[LV]>, len: u32) -> (bool, Range<Event>) {
        // History that is still as it was saved has to be read for an event that was made
        // without all of it: one with no parent held here, and not every old head for parents.
        // A block of it starts at a critical version, so an event has all that comes before
        // the block holding its latest parent.
        if let (Some(packed), Some(parents)) = (&self.packed, parents) {
            let floor = self.graph.floor() as LV;
            if !parents.iter().any(|&parent| parent >= floor) && !packed.heads.iter().all(|&head| parents.contains(&(head as LV))) {
                let block = parents.iter().max().map_or(0, |&parent| packed.block_holding(parent as Event));
                self.unpack_from(block);
            }
        }
        let lv = self.graph.len();
        // Most events follow the last one, whose number is then all the graph needs.
        let continues = lv > 0 && parents.map_or(self.graph.frontier() == [lv - 1], |parents| parents == [(lv - 1) as LV]);
        if continues {
            return (true, self.graph.push(&[lv - 1], len));
        }
        let mut at = std::mem::take(&mut self.parents);
        at.clear();
        match parents {
            Some(parents) => at.extend(parents.iter().map(|&p| Event::try_from(p).expect("parent is not a known event"))),
            None => at.extend_from_slice(self.graph.frontier()),
        }
        let events = self.graph.push(&at, len);
        self.parents = at;
        (false, events)
    }

    /// Records that `agent` made the `len` events from `lv`, numbering them from `seq` or,
    /// without one, from where its last left off.
    fn assign(&mut self, agent: AgentId, seq: Option<u32>, lv: Event, len: u32) {
        let seq = seq.unwrap_or(self.next_seq[agent as usize]);
        let next = &mut self.next_seq[agent as usize];
        *next = (*next).max(seq + len);
        match self.agent_runs.last_mut() {
            Some(r) if r.agent == agent && r.lv + r.len == lv && r.seq + r.len == seq => r.len += len,
            _ => self.agent_runs.push(AgentRun { lv, len, agent, seq }),
        }
    }

    /// Stores an op, as part of the last op when it follows it and simply carries it on:
    /// typing forwards, or holding the delete key. Returns the length of the op it is now in.
    fn store(&mut self, follows: bool, op: Op) -> u32 {
        match op.kind {
            OpKind::Ins => self.totals.0 += op.len,
            OpKind::Del => self.totals.1 += op.len,
        }
        match self.ops.last_mut() {
            Some(o) if follows && o.kind == op.kind && o.fwd && op.fwd && !o.trims
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
    fn split_op(&mut self, at: Event) {
        let i = self.op_containing(at) as usize;
        let Some(op) = self.ops.get(i).filter(|op| op.lv < at) else { return };
        let end = op.lv + op.len;
        let part = |events: Range<Event>| {
            let part = self.slice(op, &events);
            Op { lv: part.lv, len: part.len, kind: part.kind, fwd: part.fwd, trims: false, pos: part.pos, content: part.text }
        };
        let parts = [part(op.lv..at), part(at..end)];
        self.ops.splice(i..=i, parts);
        // A delete only trims straight after an insert or after another delete that does.
        for after in self.ops[i + 2..].iter_mut().take_while(|after| after.trims) {
            after.trims = false;
        }
    }

    /// Finds how many bytes the first `len` characters of the inserted text at `content` take.
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

    /// Returns the inserted text at `content` without its last `cut` characters: what a
    /// delete that [trims](Op::trims) the insert leaves of it.
    pub(crate) fn trimmed(&self, content: ByteRange, len: u32, cut: u32) -> ByteRange {
        if content.1 - content.0 == len {
            return (content.0, content.1 - cut);
        }
        (content.0, content.0 + self.prefix(content, len - cut))
    }

    /// Finds which agent made event `lv`, and its sequence number there.
    fn agent_at(&self, lv: Event) -> (AgentId, u32) {
        let run = &self.agent_runs[self.agent_runs.partition_point(|r| r.lv <= lv) - 1];
        (run.agent, run.seq + (lv - run.lv))
    }

    /// Breaks the tie between two concurrent inserts at the same place with the same neighbours:
    /// does the insert starting at event `a` go before the one starting at `b`?
    pub(crate) fn inserts_before(&self, a: Event, b: Event) -> bool {
        let (agent_a, seq_a) = self.agent_at(a);
        let (agent_b, seq_b) = self.agent_at(b);
        match self.agent_names[agent_a as usize].cmp(&self.agent_names[agent_b as usize]) {
            Ordering::Less => true,
            Ordering::Greater => false,
            Ordering::Equal => seq_a < seq_b,
        }
    }

    /// Finds, for each event in `starts`, which must ascend, the index of the op containing it:
    /// the number of ops, for the event after the last.
    pub(crate) fn ops_containing(&self, starts: impl Iterator<Item = Event>) -> Vec<OpIndex> {
        let mut op = 0;
        starts
            .map(|lv| {
                while self.ops.get(op).is_some_and(|op| op.lv + op.len <= lv) {
                    op += 1;
                }
                op as OpIndex
            })
            .collect()
    }

    /// Iterates over the part of each op that falls inside `events`. `ops` must run from the
    /// index of the op containing the first of them to that of the op containing the event
    /// after the last.
    pub(crate) fn ops(&self, ops: Range<OpIndex>, events: Range<Event>) -> impl DoubleEndedIterator<Item = OpSlice> + '_ {
        // The op containing the event after the last holds some of these too if it starts
        // before then.
        let straddles = self.ops.get(ops.end as usize).is_some_and(|op| op.lv < events.end);
        let count = if events.is_empty() { 0 } else { (ops.end - ops.start) as usize + straddles as usize };
        self.ops[ops.start as usize..][..count].iter().map(move |op| self.slice(op, &events))
    }

    /// Finds the index of the op containing event `lv`.
    pub(crate) fn op_containing(&self, lv: Event) -> OpIndex {
        self.ops.partition_point(|op| op.lv + op.len <= lv) as OpIndex
    }

    /// Returns all the inserted text, which an [`OpSlice`]'s range is a stretch of.
    pub(crate) fn content(&self) -> &str {
        &self.content
    }

    /// Returns the inserted text that an op's range refers to.
    fn text(&self, (start, end): ByteRange) -> &str {
        &self.content[start as usize..end as usize]
    }

    /// Returns what [`lengths`](Self::lengths) would count over every event.
    pub(crate) fn total_lengths(&self) -> (u32, u32) {
        self.totals
    }

    /// Counts how many characters `events` insert and how many they remove, given that they are
    /// applied as recorded. `first_op` is the index of the op containing the first of them.
    pub(crate) fn lengths(&self, first_op: OpIndex, events: Range<Event>) -> (u32, u32) {
        let (mut inserted, mut removed) = (0, 0);
        for op in &self.ops[first_op as usize..] {
            if op.lv.max(events.start) >= events.end {
                break;
            }
            let len = events.end.min(op.lv + op.len) - events.start.max(op.lv);
            // Which kind an op is cannot be predicted, so both sums are added to without
            // branching on it.
            let insert = (op.kind == OpKind::Ins) as u32;
            inserted += len * insert;
            removed += len * (1 - insert);
        }
        (inserted, removed)
    }

    /// Finds the bytes holding characters `from..to` of the inserted text at `content`.
    #[cold]
    fn cut(&self, content: ByteRange, from: u32, to: u32) -> ByteRange {
        let text = self.text(content);
        // Where each character starts, then where the text ends.
        let mut starts = text.char_indices().map(|(byte, _)| byte as u32).chain([text.len() as u32]);
        let first = starts.nth(from as usize).expect("an op is cut within its text");
        let last = if from < to { starts.nth((to - from - 1) as usize).expect("an op is cut within its text") } else { first };
        (content.0 + first, content.0 + last)
    }

    /// Cuts out the part of `op` that falls inside `events`.
    #[inline]
    fn slice(&self, op: &Op, events: &Range<Event>) -> OpSlice {
        let from = events.start.max(op.lv) - op.lv;
        let to = events.end.min(op.lv + op.len) - op.lv;
        // Nearly always the whole op, which needs no working out whatever its kind.
        if to - from == op.len {
            return OpSlice { lv: op.lv, len: op.len, kind: op.kind, fwd: op.fwd, trims: op.trims, pos: op.pos, text: op.content };
        }
        let (pos, text) = match (op.kind, op.fwd) {
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
        OpSlice { lv: op.lv + from, len: to - from, kind: op.kind, fwd: op.fwd, trims: false, pos, text }
    }
}
