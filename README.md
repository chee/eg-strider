# eg-strider

Replays a collaborative text document from its editing history, in the style of
[Eg-walker](https://arxiv.org/abs/2409.14252) (Gentle and Kleppmann). The history is a graph
of index-based inserts and deletes, and CRDT state is built only where edits were
concurrent.

## Results

![Time to merge each trace](compare/figures/time.svg)
![Peak memory while merging](compare/figures/memory-peak.svg)
![Memory kept once merged](compare/figures/memory-kept.svg)

The traces are the paper's: S have no concurrent edits, C are two people editing in real
time, A are git histories. Measured on an Apple M5 Pro. Eg-walker is diamond-types at the
revision the paper benchmarked, and Yjs is its Rust port.

## Where the speedup comes from

1. **Concurrent edits that do not touch.**
   - *Eg-walker* builds its CRDT for every stretch of history that has any concurrency,
     and walks the events through it.
   - *eg-strider* first tries offset arithmetic, as in operational transformation: an edit
     moves by the net length of the concurrent edits before it. The CRDT is built only
     when an edit lands in a stretch of text holding concurrent edits it has not seen.
   - *Why it helps:* two people typing at once are usually typing in different places. C1
     has 3,650 concurrent stretches and 25 need the CRDT; C2 has 4,825 and none do.

2. **Building the text.**
   - *Eg-walker* applies every edit, in order, to a rope.
   - *eg-strider* only works out where each edit goes while merging. It then takes the
     edits back from last to first over a piece table of the final text, copying each
     surviving character straight to its final place.
   - *Why it helps:* most of what is typed does not survive. S1 types 547k characters to
     end with 315k, and S3 types 1.2M to end with 122k. A rope copies and shifts all of
     it; here text that is later deleted is never copied and nothing is moved.

3. **Finding a record by the event that made it.**
   - *Eg-walker* keeps two B-trees while merging: one holding the CRDT's records in
     document order, and one from event ids to those records, used whenever the version
     being looked at moves backwards or forwards.
   - *eg-strider* keeps the first tree and replaces the second with a flat array, since a
     stretch's events are numbered consecutively. It also orders the walk by comparing
     versions as vectors rather than by searching the graph.
   - *Why it helps:* git histories branch and merge constantly. One stretch of A2 is 1,214
     blocks with 749 forks and 765 merges, and merging it moves the version across 1.6
     events for every event applied. Each of those is a lookup.

4. **Analysing the graph.**
   - *Eg-walker* works out where history is linear, and a plan for the rest, each time it
     merges.
   - *eg-strider* keeps that up to date as events are added, and keeps what the CRDT
     resolved so that a later checkout places those events directly.
   - *Why it helps:* real-time traces cut into many small pieces (C1 into 7,300), so
     planning is a visible share of a merge that otherwise does little.

## More state, less memory

- *Eg-walker* keeps nothing between merges but the document. While merging it holds its
  two B-trees and the document as a rope.
- *eg-strider* keeps more. The log carries where its history is linear, kept up to date as
  events arrive. A checkout also leaves behind what the CRDT resolved for each concurrent
  stretch that needed it, for the next checkout to use.
- *Why it is still smaller:*
  - What a checkout leaves behind is counted per run of typing, not per character: two
    numbers for an insert and two for a run of deleted characters where the CRDT was
    needed, plus an index of the history's blocks. That is 0.4 MB on A2 and 1.2 MB on C2,
    which is cut into far more blocks.
  - The CRDT's working state exists only for the stretches that need it, is one tree and
    one array, and is dropped before the text is built.
  - Merging records one position per edit, not the edit. The rest is read back from the
    log.
  - The document is a plain string, not a rope. S1's 0.3 MB of text takes 1.3 MB as a rope.
  - The log itself, with that analysis in it, is about half the size of diamond-types' in
    memory (C1: 7.5 MB against 17.4 MB). The charts above leave the log out for both.

Peak memory is lower on every trace, by twelve times on C1 and C2. What is kept afterwards
is lower on every trace but C2, where it is about the same.

## Layout

- `crates/strider-graph`: the event graph, where it is linear, and the order the rest is
  walked in.
- `crates/strider-tree`: the order-statistic B+ tree behind the CRDT.
- `crates/strider-text`: the piece table that assembles the final text.
- `crates/eg-strider`: the operation log and the replayer.
- `compare`: checks against diamond-types, and the benchmarks.

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
minutes, and keeps the result beside the trace.
