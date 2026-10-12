//! Glue between eg-strider and diamond-types, for testing one against the other, and
//! between diamond-types and Loro, for measuring all three on the same histories.

pub mod replicas;

use diamond_types::list::operation::ListOpKind;
use diamond_types::list::ListOpLog;
use eg_strider::OpLog;

/// Copies a diamond-types oplog into an eg-strider one, event for event, so that both
/// number events identically.
pub fn convert(dt: &ListOpLog) -> OpLog {
    let mut log = OpLog::new();
    extend(&mut log, dt, usize::MAX);
    log
}

/// Appends to `log` the events of `dt` that it does not have yet, stopping at the last
/// whole chunk that ends at or before event `limit`.
pub fn extend(log: &mut OpLog, dt: &ListOpLog, limit: usize) {
    for entry in dt.as_chunked_operation_vec() {
        if entry.span.end <= log.len() {
            continue;
        }
        if entry.span.end > limit {
            break;
        }
        assert_eq!(log.len(), entry.span.start);
        let agent = log.get_or_create_agent(dt.get_agent_name(entry.agent_span.agent));
        let mut seq = entry.agent_span.seq_range.start;
        let mut parents: Vec<usize> = entry.parents.as_ref().to_vec();
        for op in &entry.ops {
            let span = op.loc.span;
            let events = match op.kind {
                ListOpKind::Ins => {
                    assert!(op.loc.fwd, "reversed inserts are not supported");
                    let text = op.content.as_ref().expect("insert without content");
                    log.add_remote_insert(agent, seq, &parents, span.start, text)
                }
                ListOpKind::Del => log.add_remote_delete(agent, seq, &parents, span.start..span.end, op.loc.fwd),
            };
            seq += events.len();
            parents = vec![events.end - 1];
        }
    }
}
