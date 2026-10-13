//! Glue between eg-strider and diamond-types, for testing one against the other, and
//! between diamond-types and Loro, for measuring all three on the same histories.

pub mod replicas;

use diamond_types::list::operation::{ListOpKind, TextOperation};
use diamond_types::list::ListOpLog;
use eg_strider::{Edit, OpLog};
use std::ops::Range;

/// A history with its ops in memory, as one peer would hand it to another. Parents and ops
/// are each kept in one list for the whole history, so that it is not much larger than its
/// ops. Each library is handed the ops in the form its own calls take: diamond-types its
/// [`TextOperation`]s, and eg-strider its [`Edit`]s beside the text they insert.
pub struct History<Op = Edit> {
    pub agents: Vec<String>,
    pub chunks: Vec<Chunk>,
    parents: Vec<usize>,
    ops: Vec<Op>,
    /// The text of every [`Edit`] that inserts, one after another.
    text: String,
}

/// A run of events by one agent, the first with the chunk's parents and each of the others
/// following the one before.
#[derive(Clone)]
pub struct Chunk {
    pub events: Range<usize>,
    /// The agent's index in [`History::agents`].
    pub agent: usize,
    pub seq: usize,
    parents: Range<u32>,
    ops: Range<u32>,
    /// Where the text that the chunk's inserts put in is in the history's.
    text: Range<u32>,
}

impl<Op> History<Op> {
    /// Returns a chunk's parents.
    pub fn parents(&self, chunk: &Chunk) -> &[usize] {
        &self.parents[chunk.parents.start as usize..chunk.parents.end as usize]
    }

    /// Returns a chunk's ops.
    pub fn ops(&self, chunk: &Chunk) -> &[Op] {
        &self.ops[chunk.ops.start as usize..chunk.ops.end as usize]
    }
}

impl History<TextOperation> {
    /// Lists the ops of a diamond-types oplog, event for event.
    pub fn of(dt: &ListOpLog) -> Self {
        let entries = dt.as_chunked_operation_vec();
        let (mut agents, mut ids) = (Vec::new(), Vec::new());
        let mut chunks = Vec::with_capacity(entries.len());
        let mut parents = Vec::with_capacity(entries.iter().map(|entry| entry.parents.as_ref().len()).sum());
        let mut ops = Vec::with_capacity(entries.iter().map(|entry| entry.ops.len()).sum());
        for entry in entries {
            let id = entry.agent_span.agent;
            let agent = ids.iter().position(|&known| known == id).unwrap_or_else(|| {
                ids.push(id);
                agents.push(dt.get_agent_name(id).to_string());
                ids.len() - 1
            });
            let (first_parent, first_op) = (parents.len() as u32, ops.len() as u32);
            parents.extend_from_slice(entry.parents.as_ref());
            ops.extend(entry.ops);
            chunks.push(Chunk {
                events: entry.span.start..entry.span.end,
                agent,
                seq: entry.agent_span.seq_range.start,
                parents: first_parent..parents.len() as u32,
                ops: first_op..ops.len() as u32,
                text: 0..0,
            });
        }
        History { agents, chunks, parents, ops, text: String::new() }
    }

    /// Returns the same history with its ops as eg-strider is handed them.
    pub fn edits(&self) -> History {
        fn inserted(op: &TextOperation) -> &str {
            if op.kind == ListOpKind::Ins { op.content.as_ref().expect("insert without content") } else { "" }
        }
        let mut text = String::with_capacity(self.ops.iter().map(|op| inserted(op).len()).sum());
        let mut ops = Vec::with_capacity(self.ops.len());
        let chunks = self
            .chunks
            .iter()
            .map(|chunk| {
                let start = text.len() as u32;
                for op in self.ops(chunk) {
                    let (pos, len) = (op.loc.span.start as u32, (op.loc.span.end - op.loc.span.start) as u32);
                    assert!(op.kind == ListOpKind::Del || op.loc.fwd, "reversed inserts are not supported");
                    text.push_str(inserted(op));
                    ops.push(match op.kind {
                        ListOpKind::Ins => Edit::Insert { pos, len },
                        ListOpKind::Del => Edit::Delete { pos, len, backspace: !op.loc.fwd },
                    });
                }
                Chunk { text: start..text.len() as u32, ..chunk.clone() }
            })
            .collect();
        History { agents: self.agents.clone(), chunks, parents: self.parents.clone(), ops, text }
    }

    /// Returns the bytes of all the text the history inserts and deletes: what a protocol
    /// with no overhead at all would have to carry. A trace does not say what its deletes
    /// removed, so a deleted character is taken to be as long as an inserted one is on
    /// average.
    pub fn text_bytes(&self) -> usize {
        let (mut bytes, mut inserted, mut deleted) = (0, 0, 0);
        for op in &self.ops {
            let len = op.loc.span.end - op.loc.span.start;
            match op.kind {
                ListOpKind::Ins => (bytes, inserted) = (bytes + op.content.as_ref().map_or(len, |text| text.len()), inserted + len),
                ListOpKind::Del => deleted += len,
            }
        }
        bytes + (deleted * bytes).checked_div(inserted).unwrap_or(0)
    }
}

/// Copies a diamond-types oplog into an eg-strider one, event for event, so that both
/// number events identically.
pub fn convert(dt: &ListOpLog) -> OpLog {
    let mut log = OpLog::new();
    extend(&mut log, &History::of(dt).edits(), usize::MAX);
    log
}

/// Appends to `log` the events of `history` that it does not have yet, stopping at the last
/// whole chunk that ends at or before event `limit`.
pub fn extend(log: &mut OpLog, history: &History, limit: usize) {
    let agents: Vec<_> = history.agents.iter().map(|name| log.get_or_create_agent(name)).collect();
    let have = log.len();
    for chunk in history.chunks.iter().skip_while(|chunk| chunk.events.end <= have).take_while(|chunk| chunk.events.end <= limit) {
        assert_eq!(log.len(), chunk.events.start);
        let text = &history.text[chunk.text.start as usize..chunk.text.end as usize];
        log.add_remote_ops(agents[chunk.agent], chunk.seq, history.parents(chunk), history.ops(chunk), text);
    }
}
