use strider_graph::{Graph, Segment};

#[test]
fn linear_history_is_one_block_and_one_segment() {
    let mut graph = Graph::new();
    graph.push(&[], 5);
    graph.push(&[4], 3);
    let blocks = graph.blocks();
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks.events(0), 0..8);
    assert_eq!(graph.segments(), [Segment::Linear(0..1)]);
    assert_eq!(graph.frontier(), [7]);
}

#[test]
fn a_fork_and_merge_is_a_region_between_linear_stretches() {
    let mut graph = Graph::new();
    graph.push(&[], 10); // 0..10
    graph.push(&[9], 4); // 10..14, one branch
    graph.push(&[9], 2); // 14..16, the other
    assert_eq!(graph.frontier(), [13, 15]);
    graph.push(&[13, 15], 3); // 16..19, merged

    let blocks = graph.blocks();
    assert_eq!((0..blocks.len()).map(|r| blocks.events(r)).collect::<Vec<_>>(), [0..10, 10..14, 14..16, 16..19]);
    assert_eq!(blocks.parents(2), [0]);
    assert_eq!(blocks.parents(3), [1, 2]);
    assert_eq!(graph.segments(), [Segment::Linear(0..1), Segment::Region(1..3), Segment::Linear(3..4)]);
}

#[test]
fn a_parent_in_the_middle_of_a_chain_splits_it() {
    let mut graph = Graph::new();
    graph.push(&[], 10);
    graph.push(&[3], 1); // branches from event 3, part-way along
    let blocks = graph.blocks();
    assert_eq!((0..blocks.len()).map(|r| blocks.events(r)).collect::<Vec<_>>(), [0..4, 4..10, 10..11]);
    assert_eq!(blocks.parents(1), [0]);
    assert_eq!(blocks.parents(2), [0]);
    assert_eq!(graph.segments(), [Segment::Linear(0..1), Segment::Region(1..3)]);
}

#[test]
fn concurrent_roots_are_a_region() {
    let mut graph = Graph::new();
    graph.push(&[], 2);
    graph.push(&[], 2);
    assert_eq!(graph.segments(), [Segment::Region(0..2)]);
}
