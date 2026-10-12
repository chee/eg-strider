//! The operation log: every single-character insert and delete this replica knows about, in
//! the order it learned of them, with each event's parents, author and original position.

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
    /// Inserts only: where the text is, for [`OpLog::text`].
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
}

impl OpLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of events (inserted plus deleted characters).
    pub fn len(&self) -> usize {
        self.graph.len() as usize
    }

    pub fn is_empty(&self) -> bool {
        self.graph.is_empty()
    }

    pub fn get_or_create_agent(&mut self, name: &str) -> AgentId {
        if let Some(i) = self.agent_names.iter().position(|n| n == name) {
            return i as AgentId;
        }
        self.agent_names.push(name.to_string());
        self.next_seq.push(0);
        (self.agent_names.len() - 1) as AgentId
    }

    pub fn agent_name(&self, agent: AgentId) -> &str {
        &self.agent_names[agent as usize]
    }

    /// The events with no children: the version a new local edit would build on.
    pub fn frontier(&self) -> Vec<LV> {
        self.graph.frontier().iter().map(|&lv| lv as LV).collect()
    }

    /// The document produced by merging every event in the log.
    pub fn checkout(&mut self) -> String {
        let mut memo = std::mem::take(&mut self.memo);
        let doc = replay::checkout(self, &mut memo);
        self.memo = memo;
        doc
    }

    /// How many zones the last checkout could not merge by shifting alone.
    pub fn tracked_zones(&self) -> usize {
        self.memo.tracked_zones()
    }

    /// Drops everything earlier checkouts worked out. The next one starts from scratch.
    pub fn forget(&mut self) {
        self.memo = Memo::default();
    }

    /// Insert `text` at `pos` in the document as of the current frontier.
    pub fn add_insert(&mut self, agent: AgentId, pos: usize, text: &str) -> Range<LV> {
        self.push(agent, None, None, OpKind::Ins, true, pos, text.chars().count(), text)
    }

    /// Delete `range` from the document as of the current frontier.
    pub fn add_delete(&mut self, agent: AgentId, range: Range<usize>) -> Range<LV> {
        self.push(agent, None, None, OpKind::Del, true, range.start, range.len(), "")
    }

    /// Insert `text` at `pos` in the document as of version `parents`.
    pub fn add_insert_at(&mut self, agent: AgentId, parents: &[LV], pos: usize, text: &str) -> Range<LV> {
        self.push(agent, None, Some(parents), OpKind::Ins, true, pos, text.chars().count(), text)
    }

    /// Delete `range` from the document as of version `parents`. `fwd` is false when the
    /// characters were removed last-first, as backspace does.
    pub fn add_delete_at(&mut self, agent: AgentId, parents: &[LV], range: Range<usize>, fwd: bool) -> Range<LV> {
        self.push(agent, None, Some(parents), OpKind::Del, fwd, range.start, range.len(), "")
    }

    /// As [`add_insert_at`](Self::add_insert_at), for an event received from elsewhere
    /// whose sequence number for `agent` is already fixed.
    pub fn add_remote_insert(&mut self, agent: AgentId, seq: usize, parents: &[LV], pos: usize, text: &str) -> Range<LV> {
        self.push(agent, Some(seq as u32), Some(parents), OpKind::Ins, true, pos, text.chars().count(), text)
    }

    /// As [`add_delete_at`](Self::add_delete_at), with a fixed sequence number.
    pub fn add_remote_delete(&mut self, agent: AgentId, seq: usize, parents: &[LV], range: Range<usize>, fwd: bool) -> Range<LV> {
        self.push(agent, Some(seq as u32), Some(parents), OpKind::Del, fwd, range.start, range.len(), "")
    }

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

    fn agent_at(&self, lv: u32) -> (AgentId, u32) {
        let run = &self.agent_runs[self.agent_runs.partition_point(|r| r.lv <= lv) - 1];
        (run.agent, run.seq + (lv - run.lv))
    }

    /// Tie-break for two concurrent inserts at the same place with the same neighbours:
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

    /// For each event in `starts`, which must ascend, the index of the op containing it.
    pub(crate) fn ops_containing(&self, starts: impl Iterator<Item = u32>) -> Vec<u32> {
        let mut op = 0;
        starts
            .map(|lv| {
                while self.ops[op].lv + self.ops[op].len <= lv {
                    op += 1;
                }
                op as u32
            })
            .collect()
    }

    /// The part of each op that falls inside `events`. `first_op` must be the index of the
    /// op containing the first of them.
    pub(crate) fn ops(&self, first_op: u32, events: Range<u32>) -> impl DoubleEndedIterator<Item = OpSlice> + '_ {
        let ops = &self.ops[first_op as usize..];
        // Gallop to an op that starts at or after the end, then search back from it.
        let mut reach = 1;
        while reach < ops.len() && ops[reach].lv < events.end {
            reach *= 2;
        }
        // With no events there is nothing, though an op starts before where they would be.
        let ops = if events.is_empty() { &ops[..0] } else { &ops[..reach.min(ops.len())] };
        let count = ops.partition_point(|op| op.lv < events.end);
        ops[..count].iter().map(move |op| self.slice(op, &events))
    }

    pub(crate) fn text(&self, (start, end): (u32, u32)) -> &str {
        &self.content[start as usize..end as usize]
    }

    /// How many characters `events` insert and how many they remove, given that they are
    /// applied as recorded. `first_op` as for [`ops`](Self::ops).
    pub(crate) fn lengths(&self, first_op: u32, events: Range<u32>) -> (u32, u32) {
        let (mut inserted, mut removed) = (0, 0);
        for op in &self.ops[first_op as usize..] {
            if op.lv.max(events.start) >= events.end {
                break;
            }
            let len = events.end.min(op.lv + op.len) - events.start.max(op.lv);
            match op.kind {
                OpKind::Ins => inserted += len,
                OpKind::Del => removed += len,
            }
        }
        (inserted, removed)
    }

    /// The part of `op` that falls inside `events`.
    #[inline]
    fn slice(&self, op: &Op, events: &Range<u32>) -> OpSlice {
        let from = events.start.max(op.lv) - op.lv;
        let to = events.end.min(op.lv + op.len) - op.lv;
        let (pos, text) = match (op.kind, op.fwd) {
            (OpKind::Ins, _) => {
                let (start, end) = op.content;
                // Characters and bytes coincide unless the text is not all ASCII.
                let byte = |ch: u32| {
                    if end - start == op.len {
                        ch
                    } else {
                        let text = self.text(op.content);
                        text.char_indices().nth(ch as usize).map_or(text.len(), |(b, _)| b) as u32
                    }
                };
                let text = if to - from == op.len { op.content } else { (start + byte(from), start + byte(to)) };
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
