pub mod replicas;

use diamond_types::list::operation::{ListOpKind, TextOperation};
use diamond_types::list::ListOpLog;
use eg_strider::{Edit, EventIndex, OpLog, SequenceNumber};
use std::ops::Range;

/// A history with its ops in memory, as one peer would hand it to another, for testing
/// eg-strider against diamond-types and measuring every library on the same histories. Each
/// library gets the ops in the form its calls take: diamond-types its [`TextOperation`]s with
/// `usize` parents, and eg-strider its [`Edit`]s beside their text, with [`EventIndex`] parents.
pub struct History<Op = Edit, Parent = EventIndex> {
    pub agents: Vec<String>,
    pub chunks: Vec<Chunk>,
    parents: Vec<Parent>,
    ops: Vec<Op>,
    /// The text of every inserting [`Edit`], in order.
    text: String,
}

/// A run of events by one agent: the first with the chunk's parents, each other following the
/// one before.
#[derive(Clone)]
pub struct Chunk {
    pub events: Range<usize>,
    /// The agent's index in [`History::agents`].
    pub agent: usize,
    pub sequence_number: usize,
    parents: Range<u32>,
    ops: Range<u32>,
    /// Where the chunk's inserted text is in the history's.
    text: Range<u32>,
}

impl<Op, Parent> History<Op, Parent> {
    /// Returns a chunk's parents.
    pub fn parents(&self, chunk: &Chunk) -> &[Parent] {
        &self.parents[chunk.parents.start as usize..chunk.parents.end as usize]
    }

    /// Returns a chunk's ops.
    pub fn ops(&self, chunk: &Chunk) -> &[Op] {
        &self.ops[chunk.ops.start as usize..chunk.ops.end as usize]
    }

    /// Returns how many events the history has.
    pub fn events(&self) -> usize {
        self.chunks.last().map_or(0, |chunk| chunk.events.end)
    }
}

/// An editing trace from the paper's datasets: its history, and the text it should end with.
pub struct Trace {
    pub history: History<TextOperation, usize>,
    pub end_content: String,
}

impl Trace {
    /// Reads a trace in the `editing-traces` JSON format: transactions, each by an agent, with
    /// parents as indexes into the list of transactions and patches applied in order. A patch
    /// deletes characters at a position and then inserts text there, counting Unicode
    /// characters. Agents are numbers, so they are named by them, zero-padded so that names
    /// sort as the numbers do.
    pub fn of_json(json: &[u8]) -> Result<Trace, String> {
        let trace: serde_json::Value = serde_json::from_slice(json).map_err(|e| e.to_string())?;
        let field = |name: &str| trace.get(name).ok_or(format!("no {name}"));
        let txns = field("txns")?.as_array().ok_or("txns is not a list")?;
        let agents = field("numAgents")?.as_u64().ok_or("numAgents is not a number")? as usize;
        let end_content = field("endContent")?.as_str().ok_or("endContent is not text")?.to_string();
        let number = |value: &serde_json::Value| value.as_u64().map(|n| n as usize).ok_or("a number expected");

        let (mut chunks, mut parents, mut ops) = (Vec::with_capacity(txns.len()), Vec::new(), Vec::new());
        let (mut ends, mut next_sequence) = (Vec::with_capacity(txns.len()), vec![0; agents]);
        for txn in txns {
            let agent = number(txn.get("agent").ok_or("a txn without an agent")?)?;
            let start = ends.last().copied().unwrap_or(0);
            let (first_parent, first_op) = (parents.len() as u32, ops.len() as u32);
            for parent in txn.get("parents").and_then(|p| p.as_array()).ok_or("a txn without parents")? {
                parents.push(*ends.get(number(parent)?).ok_or("a parent after its child")? - 1);
            }
            let mut len = 0;
            for patch in txn.get("patches").and_then(|p| p.as_array()).ok_or("a txn without patches")? {
                let (pos, deleted) = (number(&patch[0])?, number(&patch[1])?);
                let inserted = patch[2].as_str().ok_or("a patch without text")?;
                if deleted > 0 {
                    ops.push(TextOperation::new_delete(pos..pos + deleted));
                }
                if !inserted.is_empty() {
                    ops.push(TextOperation::new_insert(pos, inserted));
                }
                len += deleted + inserted.chars().count();
            }
            let next = next_sequence.get_mut(agent).ok_or("an agent past numAgents")?;
            let sequence_number = std::mem::replace(next, *next + len);
            chunks.push(Chunk {
                events: start..start + len,
                agent,
                sequence_number,
                parents: first_parent..parents.len() as u32,
                ops: first_op..ops.len() as u32,
                text: 0..0,
            });
            ends.push(start + len);
        }
        let agents = (0..agents).map(|agent| format!("agent{agent:04}")).collect();
        Ok(Trace { history: History { agents, chunks, parents, ops, text: String::new() }, end_content })
    }
}

impl History<TextOperation, usize> {
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
                sequence_number: entry.agent_span.seq_range.start,
                parents: first_parent..parents.len() as u32,
                ops: first_op..ops.len() as u32,
                text: 0..0,
            });
        }
        History { agents, chunks, parents, ops, text: String::new() }
    }

    /// Returns the history with its ops in eg-strider's form.
    pub fn edits(&self) -> History {
        fn inserted(op: &TextOperation) -> &str {
            if op.kind == ListOpKind::Ins {
                op.content.as_ref().expect("insert without content")
            } else {
                ""
            }
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
        let parents = self.parents.iter().map(|&parent| parent as EventIndex).collect();
        History { agents: self.agents.clone(), chunks, parents, ops, text }
    }

    /// Returns the bytes of all the text the history inserts and deletes: what a protocol with
    /// no overhead would carry. Traces do not record what was deleted, so a deleted character
    /// counts as long as an inserted one on average.
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

/// Copies a diamond-types oplog into an eg-strider one, numbering events identically.
pub fn convert(dt: &ListOpLog) -> OpLog {
    let mut log = OpLog::new();
    extend(&mut log, &History::of(dt).edits(), usize::MAX);
    log
}

/// Appends the events of `history` that `log` lacks, up to the last whole chunk ending at or
/// before `limit`.
pub fn extend(log: &mut OpLog, history: &History, limit: usize) {
    let agents: Vec<_> = history.agents.iter().map(|name| log.get_or_create_agent(name)).collect();
    let have = log.len();
    for chunk in history.chunks.iter().skip_while(|chunk| chunk.events.end <= have).take_while(|chunk| chunk.events.end <= limit) {
        assert_eq!(log.len(), chunk.events.start);
        let text = &history.text[chunk.text.start as usize..chunk.text.end as usize];
        log.add_remote_ops(agents[chunk.agent], chunk.sequence_number as SequenceNumber, history.parents(chunk), history.ops(chunk), text)
            .expect("a trace's events are valid");
    }
}
