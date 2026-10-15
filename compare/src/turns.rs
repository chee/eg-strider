use crate::loaded::Loaded;
use crate::memory::{memory_of, Memory};
use crate::{
    automerge_apply, automerge_decode, automerge_open, automerge_text, dt_append, dt_open, loro_apply, loro_decode_batches, loro_import, loro_text, since,
    yjs_apply, yjs_decode, yjs_decode_batches, yjs_merge, yjs_save, yjs_text,
};
use eg_strider::OpLog;
use loro::ExportMode;
use std::time::Instant;

/// How long each trace's merges run before any is timed.
const WARM_UP: std::time::Duration = std::time::Duration::from_millis(300);

/// What a system took on the two tasks.
#[derive(Clone, Copy)]
pub(crate) struct Measured {
    pub(crate) merge_ms: f64,
    /// Memory the history's ops take, in the form the system gets them.
    pub(crate) ops: usize,
    /// While merging: peak memory, ops included, and what the merged document holds.
    pub(crate) merging: Memory,
    pub(crate) open_ms: f64,
    /// While reopening: peak memory, and what the reopened document holds.
    pub(crate) opening: Memory,
    /// Size of the saved document.
    pub(crate) saved: usize,
    /// Whether both tasks ended with diamond-types' text.
    pub(crate) same: bool,
}

/// One run of merging: the milliseconds taken, then for eg-strider and diamond-types those spent
/// appending, then for eg-strider those one more edit at the end takes. And whether the text was
/// diamond-types'.
pub(crate) type MergeRun = ([f64; 3], bool);

/// A library's two tasks on one trace, each run once per call, so that every trace takes its
/// turn.
pub(crate) struct Turns<'a> {
    pub(crate) merge: Box<dyn FnMut() -> MergeRun + 'a>,
    pub(crate) open: Box<dyn FnMut() -> f64 + 'a>,
}

/// Checks a library on the two tasks and measures their memory, then sets them up to be timed
/// in turns with its tasks on the other traces. `ops` puts a history's ops in memory, untimed but
/// counted. A document's text is read only to check it, outside the times and memory.
fn prepare<'a, O: 'a, D: 'a>(
    expected: &str,
    ops: impl Fn() -> O + 'a,
    merge: impl Fn(O) -> D + 'a,
    text: impl Fn(&D) -> String,
    save: impl Fn(&mut D) -> Vec<u8>,
    open: impl Fn(&[u8]) -> D + 'a,
) -> (Measured, Turns<'a>) {
    let mut document = merge(ops());
    let saved = save(&mut document);
    let same = text(&document) == expected && text(&open(&saved)) == expected;
    drop(document);
    let held_by_ops = memory_of(&ops).1.kept;
    let merging = memory_of(|| merge(ops())).1;
    let opening = memory_of(|| open(&saved)).1;
    let measured = Measured { merge_ms: f64::NAN, ops: held_by_ops, merging, open_ms: f64::NAN, opening, saved: saved.len(), same };
    let turns = Turns {
        merge: Box::new(move || {
            let input = ops();
            let t = Instant::now();
            let document = merge(input);
            let ms = since(t);
            drop(document);
            ([ms, f64::NAN, f64::NAN], true)
        }),
        open: Box::new(move || {
            let t = Instant::now();
            let document = open(&saved);
            let ms = since(t);
            drop(document);
            ms
        }),
    };
    (measured, turns)
}

/// Runs one library's task on every trace in turns, round after round, until each has `reps`
/// runs that count. The library's code stays warm, as in an application that merges often,
/// but no run follows one on the same trace, whose branches the processor would have learnt:
/// that makes some libraries twice as fast. The first runs of a short task are up to twice as
/// slow as the rest, until the processor has sped up, so only rounds begun once some time has
/// gone by count.
pub(crate) fn in_turns<T>(reps: u32, tasks: &mut [&mut dyn FnMut() -> T]) -> Vec<Vec<T>> {
    let mut runs: Vec<Vec<T>> = tasks.iter().map(|_| Vec::with_capacity(reps as usize)).collect();
    let started = Instant::now();
    while runs.first().is_some_and(|runs| runs.len() < reps as usize) {
        let counts = started.elapsed() >= WARM_UP;
        for (task, runs) in tasks.iter_mut().zip(&mut runs) {
            let run = task();
            if counts {
                runs.push(run);
            }
        }
    }
    runs
}

/// Sets eg-strider's two tasks on a trace up to be timed. Its merge also times its appending,
/// and one more edit at the end, merged on top of what is there.
pub(crate) fn strider_turns(loaded: &Loaded) -> Turns<'_> {
    let (history, expected) = (&loaded.history, loaded.expected.as_str());
    let expected_chars = expected.chars().count();
    Turns {
        merge: Box::new(move || {
            let t = Instant::now();
            let mut log = OpLog::new();
            compare::extend(&mut log, history, usize::MAX);
            let append_ms = since(t);
            let merged = log.checkout();
            let merge_ms = since(t);
            let mut same = merged == expected;
            let typist = log.get_or_create_replica("typist");
            let t = Instant::now();
            log.add_insert(typist, expected_chars, "!");
            let merged = log.checkout().len();
            let again_ms = since(t);
            same &= merged == expected.len() + 1;
            ([merge_ms, append_ms, again_ms], same)
        }),
        // eg-strider takes the file's bytes to keep, so each run is handed its own.
        open: Box::new(move || {
            let bytes = loaded.saved.clone();
            let t = Instant::now();
            let mut log = OpLog::load(bytes).unwrap();
            log.checkout();
            since(t)
        }),
    }
}

/// Sets diamond-types' two tasks on a trace up to be timed. Its merge also times its appending.
pub(crate) fn dt_turns(loaded: &Loaded) -> Turns<'_> {
    Turns {
        merge: Box::new(move || {
            let t = Instant::now();
            let oplog = dt_append(&loaded.dt_history);
            let append_ms = since(t);
            let branch = oplog.checkout_tip();
            let ms = since(t);
            let same = branch.content() == loaded.expected;
            ([ms, append_ms, f64::NAN], same)
        }),
        open: Box::new(move || {
            let t = Instant::now();
            let document = dt_open(&loaded.dt_saved);
            let ms = since(t);
            drop(document);
            ms
        }),
    }
}

/// Checks and weighs yrs, Loro and Automerge on a trace, and sets their tasks up to be timed.
pub(crate) fn others(loaded: &Loaded) -> [Option<(Measured, Turns<'_>)>; 3] {
    let expected = &loaded.expected;
    let [yjs, loro, automerge] = &loaded.sent;
    [
        yjs.as_ref()
            .ok()
            .map(|(sent, _)| prepare(expected, move || yjs_decode_batches(sent), yjs_merge, yjs_text, yjs_save, |saved| yjs_apply(yjs_decode(saved)))),
        loro.as_ref().ok().map(|(sent, _)| {
            let ops = loro_decode_batches(sent);
            prepare(expected, move || ops.clone(), loro_apply, loro_text, |document| document.export(ExportMode::snapshot()).unwrap(), loro_import)
        }),
        automerge.as_ref().ok().map(|(sent, _)| {
            prepare(
                expected,
                move || automerge_decode(sent),
                automerge_apply,
                automerge_text,
                |document: &mut automerge::AutoCommit| document.save(),
                automerge_open,
            )
        }),
    ]
}
