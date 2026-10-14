use strider_graph::{BlockId, Graph, Move, Segment, Walker};
use proptest::prelude::*;
use proptest::sample::Index;

/// What one of several replicas does next.
#[derive(Clone, Debug)]
enum Step {
    /// Add a chain of events to its own version.
    Extend { who: Index, events: u32 },
    /// Take everything another replica has seen into its version.
    Merge { who: Index, from: Index },
}

fn steps() -> impl Strategy<Value = Vec<Step>> {
    let step = prop_oneof![
        3 => (any::<Index>(), 1u32..4).prop_map(|(who, events)| Step::Extend { who, events }),
        1 => any::<[Index; 2]>().prop_map(|[who, from]| Step::Merge { who, from }),
    ];
    proptest::collection::vec(step, 1..120)
}

fn graph_of(replicas: usize, steps: &[Step]) -> Graph {
    let mut graph = Graph::new();
    let mut versions: Vec<Vec<u32>> = vec![Vec::new(); replicas];
    for step in steps {
        match *step {
            Step::Extend { who, events } => {
                let who = who.index(replicas);
                let parents = reduce(&graph, &versions[who]);
                versions[who] = vec![graph.push(&parents, events).end - 1];
            }
            Step::Merge { who, from } => {
                let seen = versions[from.index(replicas)].clone();
                versions[who.index(replicas)].extend(seen);
            }
        }
    }
    graph
}

/// Every event in each event's history, by brute force.
fn histories(graph: &Graph) -> Vec<Vec<bool>> {
    let blocks = graph.blocks();
    let mut of_event: Vec<Vec<bool>> = Vec::new();
    for block in 0..blocks.len() {
        let mut seen = vec![false; graph.len() as usize];
        for &p in blocks.parents(block) {
            let last = blocks.events(p).end as usize - 1;
            seen[last] = true;
            for (s, &a) in seen.iter_mut().zip(&of_event[last]) {
                *s |= a;
            }
        }
        for event in blocks.events(block) {
            of_event.push(seen.clone());
            seen[event as usize] = true;
        }
    }
    of_event
}

/// Drops from a version the events that others in it already descend from.
fn reduce(graph: &Graph, version: &[u32]) -> Vec<u32> {
    let histories = histories(graph);
    let mut reduced: Vec<u32> =
        version.iter().copied().filter(|&e| !version.iter().any(|&other| histories[other as usize][e as usize])).collect();
    reduced.sort_unstable();
    reduced.dedup();
    reduced
}

/// Follows the moves of a walk on plain sets, checking that every block is applied exactly
/// once, that the working version is then exactly the block's history, and that the blocks
/// to visit first are.
fn check_walk(graph: &Graph, walker: &mut Walker, known_quarters: usize, first_quarters: usize) {
    let blocks = graph.blocks();
    let histories = histories(graph);
    let mut applied = vec![false; blocks.len() as usize];
    for segment in graph.segments() {
        let Segment::Region(region) = segment else { continue };
        let known = region.len() * known_quarters / 4;
        let first = region.len() * first_quarters / 4;
        let mut visited = known;
        let base = blocks.start(region.start) as usize;
        let mut version = vec![false; graph.len() as usize];
        version[..blocks.start(region.start + known as BlockId) as usize].fill(true);
        applied[region.start as usize..region.start as usize + known].fill(true);

        walker.walk(blocks, &region, known, first, |step| match step {
            Move::Retreat(stretch) => {
                for e in blocks.start(stretch.start)..blocks.start(stretch.end) {
                    assert!(std::mem::replace(&mut version[e as usize], false), "retreated an event that was not there");
                }
            }
            Move::Advance(stretch) => {
                for e in blocks.start(stretch.start)..blocks.start(stretch.end) {
                    assert!(!std::mem::replace(&mut version[e as usize], true), "advanced an event that was there");
                }
            }
            Move::Apply(block) => {
                assert!(!std::mem::replace(&mut applied[block as usize], true), "block applied twice");
                assert!(visited >= first || block < region.start + first as BlockId, "block {block} applied before the first {first}");
                visited += 1;
                let first = blocks.events(block).start as usize;
                assert_eq!(version[base..], histories[first][base..], "wrong version for block {block}");
                for e in blocks.events(block) {
                    version[e as usize] = true;
                }
            }
        });
        assert!(applied[region.start as usize..region.end as usize].iter().all(|&a| a), "a block was never applied");
    }
}

proptest! {
    #[test]
    fn every_block_is_applied_at_its_parents_version(replicas in 2usize..6, steps in steps(), known_quarters in 0usize..4, first_quarters in 0usize..5) {
        let graph = graph_of(replicas, &steps);
        check_walk(&graph, &mut Walker::new(), known_quarters, first_quarters);
    }
}
