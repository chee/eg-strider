use diamond_types::list::operation::{ListOpKind, TextOperation};
use diamond_types::list::ListOpLog;
use eg_strider::{EventIndex, Operation};
use std::ops::Range;

/// A history with its ops in memory, as one peer would hand it to another, for testing
/// eg-strider against diamond-types and measuring every library on the same histories. Each
/// library gets the ops in the form its calls take: diamond-types its [`TextOperation`]s with
/// `usize` parents, and eg-strider its [`Operation`]s beside their text, with [`EventIndex`] parents.
pub struct History<OperationRun = Operation, Parent = EventIndex> {
    pub agents: Vec<String>,
    pub txns: Vec<Txn>,
    pub(crate) parents: Vec<Parent>,
    pub(crate) ops: Vec<OperationRun>,
    /// The text of every inserting [`Operation`], in order.
    pub(crate) text: String,
}

/// A transaction of the trace: a run of events by one agent, the first with the transaction's
/// parents, each other following the one before.
#[derive(Clone)]
pub struct Txn {
    pub events: Range<usize>,
    /// The agent's index in [`History::agents`].
    pub agent: usize,
    pub sequence_number: usize,
    pub(crate) parents: Range<u32>,
    pub(crate) ops: Range<u32>,
    /// Where the txn's inserted text is in the history's.
    pub(crate) text: Range<u32>,
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
