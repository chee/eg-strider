//! Glue between eg-strider and diamond-types, for testing one against the other, and
//! between diamond-types and Loro, for measuring all three on the same histories.

pub mod replicas;

use diamond_types::list::operation::{ListOpKind, TextOperation};
use diamond_types::list::ListOpLog;
use eg_strider::OpLog;
use std::ops::Range;

/// A history with its ops in memory, as one peer would hand it to another: what both
/// eg-strider and diamond-types are given to merge.
pub struct History {
    pub agents: Vec<String>,
    pub chunks: Vec<Chunk>,
}

/// A run of events by one agent, the first with the given parents and each of the others
/// following the one before.
pub struct Chunk {
    pub events: Range<usize>,
    /// The agent's index in [`History::agents`].
    pub agent: usize,
    pub seq: usize,
    pub parents: Vec<usize>,
    pub ops: Vec<TextOperation>,
}

impl History {
    /// Lists the ops of a diamond-types oplog, event for event.
    pub fn of(dt: &ListOpLog) -> Self {
        let (mut agents, mut ids) = (Vec::new(), Vec::new());
        let chunks = dt
            .as_chunked_operation_vec()
            .into_iter()
            .map(|entry| {
                let id = entry.agent_span.agent;
                let agent = ids.iter().position(|&known| known == id).unwrap_or_else(|| {
                    ids.push(id);
                    agents.push(dt.get_agent_name(id).to_string());
                    ids.len() - 1
                });
                Chunk {
                    events: entry.span.start..entry.span.end,
                    agent,
                    seq: entry.agent_span.seq_range.start,
                    parents: entry.parents.as_ref().to_vec(),
                    ops: entry.ops.into_vec(),
                }
            })
            .collect();
        History { agents, chunks }
    }
}

/// Copies a diamond-types oplog into an eg-strider one, event for event, so that both
/// number events identically.
pub fn convert(dt: &ListOpLog) -> OpLog {
    let mut log = OpLog::new();
    extend(&mut log, &History::of(dt), usize::MAX);
    log
}

/// Appends to `log` the events of `history` that it does not have yet, stopping at the last
/// whole chunk that ends at or before event `limit`.
pub fn extend(log: &mut OpLog, history: &History, limit: usize) {
    let agents: Vec<_> = history.agents.iter().map(|name| log.get_or_create_agent(name)).collect();
    let (have, mut parents) = (log.len(), Vec::new());
    for chunk in history.chunks.iter().skip_while(|chunk| chunk.events.end <= have).take_while(|chunk| chunk.events.end <= limit) {
        assert_eq!(log.len(), chunk.events.start);
        let (agent, mut seq) = (agents[chunk.agent], chunk.seq);
        parents.clear();
        parents.extend_from_slice(&chunk.parents);
        for op in &chunk.ops {
            let span = op.loc.span;
            let events = match op.kind {
                ListOpKind::Ins => {
                    assert!(op.loc.fwd, "reversed inserts are not supported");
                    let text = op.content.as_ref().expect("insert without content");
                    log.add_remote_insert(agent, seq, &parents, span.start, text)
                }
                ListOpKind::Del => log.add_remote_delete(agent, seq, &parents, span.start..span.end, op.loc.fwd),
            };
            // The next op of a chunk follows the last event of this one.
            seq += events.len();
            parents.clear();
            parents.push(events.end - 1);
        }
    }
}
