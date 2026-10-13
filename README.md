# eg-strider

Replays a collaborative text document from its editing history, the way
[Eg-walker](https://arxiv.org/abs/2409.14252) does (Gentle and Kleppmann). The history is a
graph of inserts and deletes at positions, and a CRDT is only built for the parts of it
where people edited at the same time.

## Results

![Time to merge each trace](compare/figures/time.svg)
![Peak memory while merging](compare/figures/memory-peak.svg)
![Memory held once merged](compare/figures/memory-kept.svg)

These are the paper's traces. S have no concurrent edits, C are two people editing in real
time, and A are git histories. Eg-walker here is diamond-types at the revision the paper
benchmarked, and Yjs is its Rust port. Measured on an Apple M5 Pro.

## Why it is faster

- Most concurrent edits don't need a CRDT. Eg-walker builds its CRDT for any part of the
  history that has concurrency. eg-strider first tries shifting each edit by the net length
  of the concurrent edits before it, as operational transformation would, and only falls
  back to the CRDT when an edit lands in text that holds concurrent edits it hasn't seen.
  Two people typing at the same time are usually typing in different places, so the
  fallback is rare. C1 has 3,650 concurrent parts and 25 of them need the CRDT. C2 has
  4,825 and none do.
- The text is built backwards. Eg-walker applies every edit in order to a rope. eg-strider
  only works out where each edit lands while merging, then undoes the edits from last to
  first over a piece table of the final text, copying each surviving character once to its
  final position. Most of what gets typed is later deleted (S1 types 547k characters and
  ends with 315k, S3 types 1.2M and ends with 122k), and a rope has to insert and shift
  all of it, including the text that won't be there at the end.
- Finding a CRDT record by the event that created it is an array read. Eg-walker keeps a
  second B-tree from event ids to records, and uses it every time it steps its view of the
  document back or forward by an event (the paper's retreat and advance). The events of
  one concurrent part are numbered consecutively, so eg-strider uses a flat array. This
  shows up on git histories because they branch and merge all the time. One part of A2 has
  749 forks and 765 merges, and merging it steps back or forward 1.6 events for every
  event it applies.
- The graph is analysed as events arrive. Eg-walker works out where the history is linear,
  and a plan for the rest, each time it merges. eg-strider keeps track of this on every
  append, so a replay doesn't start with a pass over the graph. C1 is cut into 7,300
  pieces, so for it that pass was a real share of the work.

## Memory

eg-strider holds on to more than Eg-walker does. The stored history also records where it
is linear, and each replay saves what the CRDT resolved so that the next replay can skip
it.
It still uses less memory, for a few reasons:

- What a replay saves is small: two numbers per insert or run of deletes where the CRDT
  was needed, plus an index into the history. That is 0.4 MB for A2's 698k events.
- The CRDT state only exists for the parts that need it, and it is freed before the text
  is built.
- While merging, each edit is stored as one position. The rest of the edit is read back
  from the history.
- The result is a string. Eg-walker holds the document as a rope, which for S1's 0.3 MB of
  text takes 1.3 MB.

Peak memory is lower on every trace, about 12 times lower on C1 and C2. Memory held
afterwards is lower on every trace except C2, where it is about the same.

## Layout

- `crates/strider-graph`: the event graph, where it is linear, and the order concurrent
  events are visited in.
- `crates/strider-tree`: the B+ tree that holds the CRDT's records.
- `crates/strider-text`: the piece table that builds the final text.
- `crates/eg-strider`: the operation log and the replay.
- `compare`: tests against diamond-types, and the benchmarks.

## Reproducing the results

Copy the paper's traces (`datasets/*.dt` from
[josephg/egwalker-paper](https://github.com/josephg/egwalker-paper)) into `traces/`, then:

```bash
cargo run --release -p compare -- --replay --reps 25 --json compare/figures/results.json traces/*.dt
```

```bash
python3 compare/figures.py compare/figures/results.json compare/figures
```

The first run also replays each history into Yjs, Loro and Automerge, which takes several
minutes. The results are saved next to the trace.
