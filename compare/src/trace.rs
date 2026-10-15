use crate::history::{History, Txn};
use diamond_types::list::operation::TextOperation;

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
