use strider_graph::{Graph, Move, Segment, Walker};
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
    let runs = graph.runs();
    let mut of_event: Vec<Vec<bool>> = Vec::new();
    for run in 0..runs.len() {
        let mut seen = vec![false; graph.len() as usize];
        for &p in runs.parents(run) {
            let last = runs.events(p as usize).end as usize - 1;
            seen[last] = true;
            for (s, &a) in seen.iter_mut().zip(&of_event[last]) {
                *s |= a;
            }
        }
        for event in runs.events(run) {
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

/// Follows the moves of a walk on plain sets, checking that every run is applied exactly
/// once and that the working version is then exactly the run's history.
fn check_walk(graph: &Graph, walker: &mut Walker, known_quarters: usize) {
    let runs = graph.runs();
    let histories = histories(graph);
    let mut applied = vec![false; runs.len()];
    for segment in runs.segments() {
        let Segment::Zone(zone) = segment else { continue };
        let known = zone.len() * known_quarters / 4;
        let base = runs.start(zone.start) as usize;
        let mut version = vec![false; graph.len() as usize];
        version[..runs.start(zone.start + known) as usize].fill(true);
        applied[zone.start..zone.start + known].fill(true);

        walker.walk(&runs, &zone, known, |step| match step {
            Move::Retreat(events) => {
                for e in events {
                    assert!(std::mem::replace(&mut version[e as usize], false), "retreated an event that was not there");
                }
            }
            Move::Advance(events) => {
                for e in events {
                    assert!(!std::mem::replace(&mut version[e as usize], true), "advanced an event that was there");
                }
            }
            Move::Apply(run) => {
                assert!(!std::mem::replace(&mut applied[run], true), "run applied twice");
                let first = runs.events(run).start as usize;
                assert_eq!(version[base..], histories[first][base..], "wrong version for run {run}");
                for e in runs.events(run) {
                    version[e as usize] = true;
                }
            }
        });
        assert!(applied[zone.start..zone.end].iter().all(|&a| a), "a run was never applied");
    }
}

proptest! {
    #[test]
    fn every_run_is_applied_at_its_parents_version(replicas in 2usize..6, steps in steps(), known_quarters in 0usize..4) {
        let graph = graph_of(replicas, &steps);
        check_walk(&graph, &mut Walker::new(), known_quarters);
        // A limit of 0 forces the path that searches the graph for each move.
        check_walk(&graph, &mut Walker::with_bitset_limit(0), known_quarters);
    }
}
