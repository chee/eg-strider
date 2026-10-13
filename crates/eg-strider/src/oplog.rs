//! The operation log: every single-character insert and delete this replica knows of, in
//! the order it learned of them, with each event's parents, author and original position.
//! This is all that is stored or sent; the document is derived from it by a checkout.
//! Consecutive events are run-length encoded into ops (typing a word is one op), and
//! inserted text lives in one append-only buffer that ops and edits point into, as in a
//! piece table.

use crate::replay::{self, Memo};
use std::cmp::Ordering;
use std::ops::Range;
use strider_graph::Graph;

/// A local version: the number of an event in one replica's log. Numbers are not shared
/// between replicas; any numbering that puts parents before children is valid.
pub type LV = usize;
pub type AgentId = u32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OpKind {
    Ins,
    Del,
}

/// A run of consecutive single-character events of one kind.
struct Op {
    lv: u32,
    len: u32,
    kind: OpKind,
    /// Deletes only: false for backspace, where the first event removes the last position.
    fwd: bool,
    /// The run touches document positions `pos..pos + len`, measured before its first
    /// event.
    pos: u32,
    /// Inserts only: byte range of the inserted text in `OpLog::content`.
    content: (u32, u32),
}

/// A consecutive run of events by one agent.
struct AgentRun {
    lv: u32,
    len: u32,
    agent: AgentId,
    seq: u32,
}

/// Part of one op, as handed to the replayer.
#[derive(Clone, Copy)]
pub(crate) struct OpSlice {
    pub lv: u32,
    pub len: u32,
    pub kind: OpKind,
    pub fwd: bool,
    /// Inserts go at `pos`; deletes remove `pos..pos + len`.
    pub pos: u32,
    /// Inserts only: where the text is in [`OpLog::content`], in bytes.
    pub text: (u32, u32),
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
    /// Reused to hold the parents of the event being added.
    parents: Vec<u32>,
    /// How many characters the inserts put in, and how many the deletes take out.
    totals: (u32, u32),
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

    /// Merges every event in the log and returns the document.
    pub fn checkout(&mut self) -> String {
        let mut memo = std::mem::take(&mut self.memo);
        let doc = replay::checkout(self, &mut memo);
        self.memo = memo;
        doc
    }

    /// Returns how many regions the last checkout could not merge by shifting alone.
    pub fn tracked_regions(&self) -> usize {
        self.memo.tracked_regions()
    }

    /// Drops everything earlier checkouts worked out. The next one starts from scratch.
    pub fn forget(&mut self) {
        self.memo = Memo::default();
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

    /// Appends a run of events of one kind, extending the last op when it simply carries on.
    #[allow(clippy::too_many_arguments)]
    fn push(&mut self, agent: AgentId, seq: Option<u32>, parents: Option<&[LV]>, kind: OpKind, fwd: bool, pos: usize, len: usize, text: &str) -> Range<LV> {
        let lv = self.graph.len();
        if len == 0 {
            return lv as LV..lv as LV;
        }
        let len = u32::try_from(len).expect("run too long");
        let pos = u32::try_from(pos).expect("position too large");
        // No parents given: the event is made at the current frontier.
        let mut at = std::mem::take(&mut self.parents);
        at.clear();
        match parents {
            Some(parents) => at.extend(parents.iter().map(|&p| u32::try_from(p).expect("parent is not a known event"))),
            None => at.extend_from_slice(self.graph.frontier()),
        }
        let continues = self.graph.continues(&at);
        let events = self.graph.push(&at, len);
        self.parents = at;

        let seq = seq.unwrap_or(self.next_seq[agent as usize]);
        let next = &mut self.next_seq[agent as usize];
        *next = (*next).max(seq + len);
        match self.agent_runs.last_mut() {
            Some(r) if r.agent == agent && r.lv + r.len == lv && r.seq + r.len == seq => r.len += len,
            _ => self.agent_runs.push(AgentRun { lv, len, agent, seq }),
        }

        match kind {
            OpKind::Ins => self.totals.0 += len,
            OpKind::Del => self.totals.1 += len,
        }
        let at = self.content.len() as u32;
        self.content.push_str(text);
        let content = (at, self.content.len() as u32);
        match self.ops.last_mut() {
            // Typing forwards, or holding the delete key: extend the previous run.
            Some(o) if continues && o.kind == kind && o.fwd && fwd
                && ((kind == OpKind::Ins && o.pos + o.len == pos) || (kind == OpKind::Del && o.pos == pos)) =>
            {
                o.len += len;
                o.content.1 = content.1;
            }
            _ => self.ops.push(Op { lv, len, kind, fwd, pos, content }),
        }
        events.start as LV..events.end as LV
    }

    /// Finds which agent made event `lv`, and its sequence number there.
    fn agent_at(&self, lv: u32) -> (AgentId, u32) {
        let run = &self.agent_runs[self.agent_runs.partition_point(|r| r.lv <= lv) - 1];
        (run.agent, run.seq + (lv - run.lv))
    }

    /// Breaks the tie between two concurrent inserts at the same place with the same neighbours:
    /// does the insert starting at event `a` go before the one starting at `b`?
    pub(crate) fn inserts_before(&self, a: u32, b: u32) -> bool {
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
    pub(crate) fn ops_containing(&self, starts: impl Iterator<Item = u32>) -> Vec<u32> {
        let mut op = 0;
        starts
            .map(|lv| {
                while self.ops.get(op).is_some_and(|op| op.lv + op.len <= lv) {
                    op += 1;
                }
                op as u32
            })
            .collect()
    }

    /// Iterates over the part of each op that falls inside `events`. `ops` must run from the
    /// index of the op containing the first of them to that of the op containing the event
    /// after the last.
    pub(crate) fn ops(&self, ops: Range<u32>, events: Range<u32>) -> impl DoubleEndedIterator<Item = OpSlice> + '_ {
        // The op containing the event after the last holds some of these too if it starts
        // before then.
        let straddles = self.ops.get(ops.end as usize).is_some_and(|op| op.lv < events.end);
        let count = if events.is_empty() { 0 } else { (ops.end - ops.start) as usize + straddles as usize };
        self.ops[ops.start as usize..][..count].iter().map(move |op| self.slice(op, &events))
    }

    /// Returns all the inserted text, which an [`OpSlice`]'s range is a stretch of.
    pub(crate) fn content(&self) -> &str {
        &self.content
    }

    /// Returns the inserted text that an op's range refers to.
    fn text(&self, (start, end): (u32, u32)) -> &str {
        &self.content[start as usize..end as usize]
    }

    /// Returns what [`lengths`](Self::lengths) would count over every event.
    pub(crate) fn total_lengths(&self) -> (u32, u32) {
        self.totals
    }

    /// Counts how many characters `events` insert and how many they remove, given that they are
    /// applied as recorded. `first_op` is the index of the op containing the first of them.
    pub(crate) fn lengths(&self, first_op: u32, events: Range<u32>) -> (u32, u32) {
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
    fn cut(&self, content: (u32, u32), from: u32, to: u32) -> (u32, u32) {
        let text = self.text(content);
        // Where each character starts, then where the text ends.
        let mut starts = text.char_indices().map(|(byte, _)| byte as u32).chain([text.len() as u32]);
        let first = starts.nth(from as usize).expect("an op is cut within its text");
        let last = if from < to { starts.nth((to - from - 1) as usize).expect("an op is cut within its text") } else { first };
        (content.0 + first, content.0 + last)
    }

    /// Cuts out the part of `op` that falls inside `events`.
    #[inline]
    fn slice(&self, op: &Op, events: &Range<u32>) -> OpSlice {
        let from = events.start.max(op.lv) - op.lv;
        let to = events.end.min(op.lv + op.len) - op.lv;
        // Nearly always the whole op, which needs no working out whatever its kind.
        if to - from == op.len {
            return OpSlice { lv: op.lv, len: op.len, kind: op.kind, fwd: op.fwd, pos: op.pos, text: op.content };
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
        OpSlice { lv: op.lv + from, len: to - from, kind: op.kind, fwd: op.fwd, pos, text }
    }
}
