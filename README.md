# eg-strider

Replays a collaborative text document from its editing history, in the style of
[Eg-walker](https://arxiv.org/abs/2409.14252) (Gentle and Kleppmann). The history is a graph
of index-based inserts and deletes, and CRDT state is built only where edits were
concurrent. It produces the same documents as the paper's implementation, 2 to 12 times
faster.

## Results

Time to build the document from its whole history, and the most memory used on top of the
history, for the paper's seven traces.

| Trace | Events | eg-strider | Eg-walker | Speedup | Peak memory |
|---|---|---|---|---|---|
| S1 | 779k | 0.76 ms | 1.70 ms | 2.2× | 0.3 MB vs 1.6 MB |
| S2 | 1,105k | 1.10 ms | 2.46 ms | 2.2× | 0.2 MB vs 1.0 MB |
| S3 | 2,339k | 0.62 ms | 3.21 ms | 5.1× | 0.3 MB vs 0.8 MB |
| C1 | 652k | 3.57 ms | 35.98 ms | 10.1× | 3.3 MB vs 40.7 MB |
| C2 | 608k | 4.37 ms | 53.77 ms | 12.3× | 3.7 MB vs 48.1 MB |
| A1 | 947k | 2.95 ms | 7.24 ms | 2.5× | 2.0 MB vs 2.4 MB |
| A2 | 698k | 7.00 ms | 20.62 ms | 2.9× | 4.4 MB vs 4.6 MB |

![Time to merge each trace](compare/figures/time.svg)

Measured on an Apple M5 Pro. Eg-walker is diamond-types at the revision the paper
benchmarked, and both are timed producing the text as a string. Memory charts are in
[`compare/figures`](compare/figures).

## Where the speedup comes from

1. **Concurrent edits that do not touch need no CRDT.** History is cut at critical versions,
   as in Eg-walker, and stretches with no concurrency are applied as recorded. A concurrent
   region is first tried with offset arithmetic, as in operational transformation: an edit
   moves by the net length of the concurrent edits before it. The CRDT is built only where
   concurrent edits land in the same stretch of text. In the real-time traces (C1, C2) that
   almost never happens.
2. **The text is built backwards, without a rope.** Merging only works out where each edit
   goes. The edits are then taken back from last to first over a piece table of the final
   text, so each surviving character is copied once, straight to its final place, and text
   that was later deleted is never copied. This is all the sequential traces (S) rely on.
3. **A leaner CRDT where one is needed.** One B+ tree counts both versions of the document,
   a flat array maps events to spans in place of a second tree, and versions are compared
   as vectors over chains of blocks. The asynchronous traces (A) are concurrent nearly
   everywhere and lean on this.
4. **Nothing is worked out twice.** Blocks and critical versions are kept up to date as
   events arrive, and what the CRDT resolved is kept at each critical version, so a later
   checkout places those events directly.

## Layout

- `crates/strider-graph`: the event graph, its critical versions, and the order a
  concurrent region is walked in.
- `crates/strider-tree`: the order-statistic B+ tree behind the CRDT.
- `crates/strider-text`: the piece table that assembles the final text.
- `crates/eg-strider`: the operation log and the replayer.
- `compare`: property tests against diamond-types, and the benchmarks.

## Running

Tests need nothing else. The benchmarks need the paper's traces (`datasets/*.dt` from
[josephg/egwalker-paper](https://github.com/josephg/egwalker-paper)) copied into `traces/`.

```bash
cargo test --workspace
cargo run --release -p compare -- --replay --json compare/figures/results.json traces/*.dt
python3 compare/figures.py compare/figures/results.json compare/figures
```

MIT licensed.
