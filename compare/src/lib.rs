pub mod replicas;

use diamond_types::list::operation::{ListOpKind, TextOperation};
use diamond_types::list::ListOpLog;
use eg_strider::{EventIndex, OpLog, Operation, SequenceNumber};
use std::ops::Range;

/// A history with its ops in memory, as one peer would hand it to another, for testing
/// eg-strider against diamond-types and measuring every library on the same histories. Each
/// library gets the ops in the form its calls take: diamond-types its [`TextOperation`]s with
/// `usize` parents, and eg-strider its [`Operation`]s beside their text, with [`EventIndex`] parents.
pub struct History<OperationRun = Operation, Parent = EventIndex> {
    pub agents: Vec<String>,
    pub txns: Vec<Txn>,
    parents: Vec<Parent>,
    ops: Vec<OperationRun>,
    /// The text of every inserting [`Operation`], in order.
    text: String,
}

/// A transaction of the trace: a run of events by one agent, the first with the transaction's
/// parents, each other following the one before.
#[derive(Clone)]
pub struct Txn {
    pub events: Range<usize>,
    /// The agent's index in [`History::agents`].
    pub agent: usize,
    pub sequence_number: usize,
    parents: Range<u32>,
    ops: Range<u32>,
    /// Where the txn's inserted text is in the history's.
    text: Range<u32>,
}

impl<OperationRun, Parent> History<OperationRun, Parent> {
    /// Returns a txn's parents.
    pub fn parents(&self, txn: &Txn) -> &[Parent] {
        &self.parents[txn.parents.start as usize..txn.parents.end as usize]
    }

    /// Returns a txn's ops.
    pub fn ops(&self, txn: &Txn) -> &[OperationRun] {
        &self.ops[txn.ops.start as usize..txn.ops.end as usize]
    }

    /// Returns how many events the history has.
    pub fn events(&self) -> usize {
        self.txns.last().map_or(0, |txn| txn.events.end)
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
        let json_txns = field("txns")?.as_array().ok_or("txns is not a list")?;
        let agents = field("numAgents")?.as_u64().ok_or("numAgents is not a number")? as usize;
        let end_content = field("endContent")?.as_str().ok_or("endContent is not text")?.to_string();
        let number = |value: &serde_json::Value| value.as_u64().map(|n| n as usize).ok_or("a number expected");

        let (mut txns, mut parents, mut ops) = (Vec::with_capacity(json_txns.len()), Vec::new(), Vec::new());
        let (mut ends, mut next_sequence) = (Vec::with_capacity(json_txns.len()), vec![0; agents]);
        for json_txn in json_txns {
            let agent = number(json_txn.get("agent").ok_or("a txn without an agent")?)?;
            let start = ends.last().copied().unwrap_or(0);
            let (first_parent, first_op) = (parents.len() as u32, ops.len() as u32);
            for parent in json_txn.get("parents").and_then(|p| p.as_array()).ok_or("a txn without parents")? {
                parents.push(*ends.get(number(parent)?).ok_or("a parent after its child")? - 1);
            }
            let mut len = 0;
            for patch in json_txn.get("patches").and_then(|p| p.as_array()).ok_or("a txn without patches")? {
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
            txns.push(Txn {
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
        Ok(Trace { history: History { agents, txns, parents, ops, text: String::new() }, end_content })
    }
}

impl History<TextOperation, usize> {
    /// Lists the ops of a diamond-types oplog, event for event.
    pub fn of(dt: &ListOpLog) -> Self {
        let entries = dt.as_chunked_operation_vec();
        let (mut agents, mut ids) = (Vec::new(), Vec::new());
        let mut txns = Vec::with_capacity(entries.len());
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
            txns.push(Txn {
                events: entry.span.start..entry.span.end,
                agent,
                sequence_number: entry.agent_span.seq_range.start,
                parents: first_parent..parents.len() as u32,
                ops: first_op..ops.len() as u32,
                text: 0..0,
            });
        }
        History { agents, txns, parents, ops, text: String::new() }
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
        let txns = self
            .txns
            .iter()
            .map(|txn| {
                let start = text.len() as u32;
                for op in self.ops(txn) {
                    let (pos, len) = (op.loc.span.start as u32, (op.loc.span.end - op.loc.span.start) as u32);
                    assert!(op.kind == ListOpKind::Del || op.loc.fwd, "reversed inserts are not supported");
                    text.push_str(inserted(op));
                    ops.push(match op.kind {
                        ListOpKind::Ins => Operation::Insert { pos, len },
                        ListOpKind::Del => Operation::Delete { pos, len, backward: !op.loc.fwd },
                    });
                }
                Txn { text: start..text.len() as u32, ..txn.clone() }
            })
            .collect();
        let parents = self.parents.iter().map(|&parent| parent as EventIndex).collect();
        History { agents: self.agents.clone(), txns, parents, ops, text }
    }
}

/// Copies a diamond-types oplog into an eg-strider one, numbering events identically.
pub fn convert(dt: &ListOpLog) -> OpLog {
    let mut log = OpLog::new();
    extend(&mut log, &History::of(dt).edits(), usize::MAX);
    log
}

/// Appends the events of `history` that `log` lacks, up to the last whole transaction ending at or
/// before `limit`.
pub fn extend(log: &mut OpLog, history: &History, limit: usize) {
    let agents: Vec<_> = history.agents.iter().map(|name| log.get_or_create_replica(name)).collect();
    let have = log.len();
    for txn in history.txns.iter().skip_while(|txn| txn.events.end <= have).take_while(|txn| txn.events.end <= limit) {
        assert_eq!(log.len(), txn.events.start);
        let text = &history.text[txn.text.start as usize..txn.text.end as usize];
        log.add_remote_ops(agents[txn.agent], txn.sequence_number as SequenceNumber, history.parents(txn), history.ops(txn), text)
            .expect("a trace's events are valid");
    }
}
