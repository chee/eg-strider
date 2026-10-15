use compare::Trace;
use diamond_types::list::ListOpLog;
use eg_strider::OpLog;

/// Two agents: one types "hello" and then " world", while the other capitalises the "h"
/// concurrently, and then adds "!" once it has seen both.
const TRACE: &str = r#"{
    "kind": "concurrent",
    "endContent": "Hello world!",
    "numAgents": 2,
    "txns": [
        {"parents": [], "numChildren": 2, "agent": 0, "patches": [[0, 0, "hello"]]},
        {"parents": [0], "numChildren": 1, "agent": 0, "patches": [[5, 0, " world"]]},
        {"parents": [0], "numChildren": 1, "agent": 1, "patches": [[0, 1, "H"]]},
        {"parents": [1, 2], "numChildren": 0, "agent": 1, "patches": [[11, 0, "!"]]}
    ]
}"#;

#[test]
fn a_trace_numbers_events_in_transaction_order() {
    let trace = Trace::of_json(TRACE.as_bytes()).unwrap();
    let history = &trace.history;
    assert_eq!(history.events(), 14);
    let txns: Vec<_> = history.txns.iter().map(|txn| (txn.events.clone(), txn.agent, txn.sequence_number, history.parents(txn).to_vec())).collect();
    assert_eq!(txns, [(0..5, 0, 0, vec![]), (5..11, 0, 5, vec![4]), (11..13, 1, 0, vec![4]), (13..14, 1, 2, vec![10, 12])]);
}

#[test]
fn a_trace_merges_to_its_text() {
    let trace = Trace::of_json(TRACE.as_bytes()).unwrap();
    let mut dt = ListOpLog::new();
    let agents: Vec<_> = trace.history.agents.iter().map(|name| dt.get_or_create_agent_id(name)).collect();
    for txn in &trace.history.txns {
        dt.add_operations_remote(agents[txn.agent], trace.history.parents(txn), txn.sequence_number, trace.history.ops(txn));
    }
    assert_eq!(dt.checkout_tip().content().to_string(), trace.end_content);

    let mut log = OpLog::new();
    compare::extend(&mut log, &trace.history.edits(), usize::MAX);
    assert_eq!(log.checkout(), trace.end_content);
}

#[test]
fn a_parent_after_its_child_is_refused() {
    let trace = TRACE.replace(r#"{"parents": [0], "numChildren": 1, "agent": 0"#, r#"{"parents": [3], "numChildren": 1, "agent": 0"#);
    assert!(Trace::of_json(trace.as_bytes()).is_err());
}
