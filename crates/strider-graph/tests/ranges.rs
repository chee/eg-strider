use strider_graph::{CausalGraph, HistoryRange};

#[test]
fn linear_history_is_one_run_and_one_range() {
    let mut graph = CausalGraph::new();
    graph.push(&[], 5);
    graph.push(&[4], 3);
    let runs = graph.runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs.events(0), 0..8);
    assert_eq!(graph.split_at_critical_versions(), [HistoryRange::Linear(0..1)]);
    assert_eq!(graph.frontier(), [7]);
}

#[test]
fn a_fork_and_merge_is_a_concurrent_range_between_linear_ones() {
    let mut graph = CausalGraph::new();
    graph.push(&[], 10); // 0..10
    graph.push(&[9], 4); // 10..14, one branch
    graph.push(&[9], 2); // 14..16, the other
    assert_eq!(graph.frontier(), [13, 15]);
    graph.push(&[13, 15], 3); // 16..19, merged

    let runs = graph.runs();
    assert_eq!((0..runs.len()).map(|r| runs.events(r)).collect::<Vec<_>>(), [0..10, 10..14, 14..16, 16..19]);
    assert_eq!(runs.parents(2), [0]);
    assert_eq!(runs.parents(3), [1, 2]);
    assert_eq!(graph.split_at_critical_versions(), [HistoryRange::Linear(0..1), HistoryRange::Concurrent(1..3), HistoryRange::Linear(3..4)]);
}

#[test]
fn a_parent_in_the_middle_of_a_chain_splits_it() {
    let mut graph = CausalGraph::new();
    graph.push(&[], 10);
    graph.push(&[3], 1); // branches from event 3, part-way along
    let runs = graph.runs();
    assert_eq!((0..runs.len()).map(|r| runs.events(r)).collect::<Vec<_>>(), [0..4, 4..10, 10..11]);
    assert_eq!(runs.parents(1), [0]);
    assert_eq!(runs.parents(2), [0]);
    assert_eq!(graph.split_at_critical_versions(), [HistoryRange::Linear(0..1), HistoryRange::Concurrent(1..3)]);
}

#[test]
fn concurrent_roots_are_a_concurrent_range() {
    let mut graph = CausalGraph::new();
    graph.push(&[], 2);
    graph.push(&[], 2);
    assert_eq!(graph.split_at_critical_versions(), [HistoryRange::Concurrent(0..2)]);
}
