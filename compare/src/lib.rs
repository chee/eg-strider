mod history;
pub mod replicas;
mod trace;

pub use history::{History, Txn};
pub use trace::Trace;

use diamond_types::list::ListOpLog;
use eg_strider::{OpLog, SequenceNumber};

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
