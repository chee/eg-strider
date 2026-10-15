use compare::replicas::TEXT;
use compare::{History, Trace};
use diamond_types::list::encoding::EncodeOptions;
use diamond_types::list::operation::TextOperation;
use diamond_types::list::{ListBranch, ListOpLog};
use eg_strider::{EventIndex, OpLog};
use jumprope::JumpRope;
use loro::{ExportMode, LoroDoc};
use std::alloc::{GlobalAlloc, Layout, System};
use std::fmt::Display;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::Instant;

/// The system allocator, counting the bytes in use.
struct Counting;

static IN_USE: AtomicUsize = AtomicUsize::new(0);
static MOST: AtomicUsize = AtomicUsize::new(0);

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Counts `bytes` more in use, raising the high-water mark if it is passed.
fn took(bytes: usize) {
    MOST.fetch_max(IN_USE.fetch_add(bytes, Relaxed) + bytes, Relaxed);
}

// SAFETY: every request is passed to the system allocator unchanged.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        took(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        took(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        IN_USE.fetch_sub(layout.size(), Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        IN_USE.fetch_sub(layout.size(), Relaxed);
        took(new_size);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

/// What running something did to memory use, in bytes.
#[derive(Clone, Copy)]
struct Memory {
    /// The most in use at once, beyond what was in use at the start.
    peak: usize,
    /// How much more is in use afterwards, while the result is held.
    kept: usize,
}

/// Runs `f` and reports what it did to the memory in use.
fn memory_of<T>(f: impl FnOnce() -> T) -> (T, Memory) {
    let before = IN_USE.load(Relaxed);
    MOST.store(before, Relaxed);
    let result = f();
    (result, Memory { peak: MOST.load(Relaxed) - before, kept: IN_USE.load(Relaxed).saturating_sub(before) })
}

/// How long each trace's merges run before any is timed.
const WARM_UP: std::time::Duration = std::time::Duration::from_millis(300);

/// How long replaying a history into another library may take before it is given up.
const LIMIT: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// Where histories replayed into other libraries are kept, as the batches each writer sent,
/// apart from the downloaded traces.
const REPLAYED: &str = "traces/batches";

/// Loads a history as the batches each writer's replica of another library sent it as, with how
/// many peers it took and how many edits had to be moved. Replayed on first use and kept in
/// [`REPLAYED`]. Nothing for a library in `left_out`.
fn replayed<R: compare::replicas::Replica>(
    name: &str,
    kind: &str,
    history: &History<TextOperation, usize>,
    replay: bool,
    left_out: &[&str],
) -> Result<(Vec<u8>, String), &'static str> {
    if left_out.contains(&kind) {
        return Err("left out");
    }
    let file = |suffix: &str| format!("{REPLAYED}/{name}.{kind}{suffix}");
    if let (Ok(bytes), Ok(notes)) = (std::fs::read(file("")), std::fs::read_to_string(file("-notes"))) {
        return Ok((bytes, notes));
    }
    if std::path::Path::new(&file("-unfinished")).exists() {
        return Err("did not finish");
    }
    if !replay {
        return Err("not replayed yet: pass --replay");
    }
    eprintln!("replaying {name} into {kind} (once)...");
    std::fs::create_dir_all(REPLAYED).unwrap();
    let Some(replayed) = compare::replicas::replay::<R>(history, LIMIT) else {
        std::fs::write(file("-unfinished"), "").unwrap();
        return Err("did not finish");
    };
    let notes = format!("{:>6} {:>6}", replayed.peers, replayed.moved);
    std::fs::write(file(""), &replayed.batches).unwrap();
    std::fs::write(file("-notes"), &notes).unwrap();
    Ok((replayed.batches, notes))
}

/// Appends a history to a new eg-strider log through its remote-edit calls, then merges it.
fn strider_merge(history: &History) -> OpLog {
    let mut log = OpLog::new();
    compare::extend(&mut log, history, usize::MAX);
    log.checkout();
    log
}

/// Appends a history to a new diamond-types oplog through its call for remote edits.
fn dt_append(history: &History<TextOperation, usize>) -> ListOpLog {
    let mut oplog = ListOpLog::new();
    let agents: Vec<_> = history.agents.iter().map(|name| oplog.get_or_create_agent_id(name)).collect();
    for txn in &history.txns {
        oplog.add_operations_remote(agents[txn.agent], history.parents(txn), txn.sequence_number, history.ops(txn));
    }
    oplog
}

/// Appends a history to a new diamond-types oplog, then merges it.
fn dt_merge(history: &History<TextOperation, usize>) -> (ListOpLog, ListBranch) {
    let oplog = dt_append(history);
    let branch = oplog.checkout_tip();
    (oplog, branch)
}

/// Puts together a saved diamond-types document: its history file's length, the file, and the
/// document's text, compressed as eg-strider's is. The paper keeps the text beside the history
/// so as not to merge again.
fn dt_save(oplog: &ListOpLog, text: &str) -> Vec<u8> {
    let file = oplog.encode(&EncodeOptions::default());
    let lens = [file.len() as u64, text.len() as u64].map(u64::to_le_bytes).concat();
    [&lens[..], &file, &zstd::bulk::compress(text.as_bytes(), 3).unwrap()].concat()
}

/// Reads a saved diamond-types document: its history, and its text as a rope.
fn dt_open(saved: &[u8]) -> (ListOpLog, JumpRope) {
    let (lens, rest) = saved.split_first_chunk::<16>().unwrap();
    let [file_len, text_len] = [0, 8].map(|at| u64::from_le_bytes(lens[at..at + 8].try_into().unwrap()) as usize);
    let (file, text) = rest.split_at(file_len);
    let text = String::from_utf8(zstd::bulk::decompress(text, text_len).unwrap()).unwrap();
    (ListOpLog::load_from(file).unwrap(), JumpRope::from(text.as_str()))
}

/// Splits batches kept each after its length.
fn batches(mut bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    std::iter::from_fn(move || {
        let (len, rest) = bytes.split_first_chunk::<4>()?;
        let (batch, rest) = rest.split_at(u32::from_le_bytes(*len) as usize);
        bytes = rest;
        Some(batch)
    })
}

/// Reads Automerge changes into memory without applying them.
fn automerge_decode(bytes: &[u8]) -> Vec<automerge::Change> {
    batches(bytes).map(|raw| automerge::Change::try_from(raw).unwrap()).collect()
}

/// Returns an Automerge document with its text.
fn automerge_text(document: &automerge::AutoCommit) -> String {
    use automerge::ReadDoc;
    let (_, text) = document.get(automerge::ROOT, "text").unwrap().unwrap();
    document.text(&text).unwrap()
}

/// Applies changes to a new Automerge document, all at once.
fn automerge_apply(changes: Vec<automerge::Change>) -> automerge::AutoCommit {
    let mut document = automerge::AutoCommit::new();
    document.apply_changes(changes).unwrap();
    document
}

/// Loads a saved Automerge document.
fn automerge_open(saved: &[u8]) -> automerge::AutoCommit {
    automerge::AutoCommit::load(saved).unwrap()
}

/// Reads a saved Yjs document, one update of its whole state, into memory without applying it.
fn yjs_decode(bytes: &[u8]) -> yrs::Update {
    use yrs::updates::decoder::Decode;
    yrs::Update::decode_v2(bytes).unwrap()
}

/// Reads the updates each writer sent into memory without applying them.
fn yjs_decode_batches(bytes: &[u8]) -> Vec<yrs::Update> {
    use yrs::updates::decoder::Decode;
    batches(bytes).map(|update| yrs::Update::decode_v1(update).unwrap()).collect()
}

/// Applies an update to a new Yjs document and returns it with its text. Garbage collection is
/// off, so that it keeps the text of deleted characters as every other library does.
fn yjs_apply(update: yrs::Update) -> yrs::Doc {
    use yrs::Transact;
    let document = yrs::Doc::with_options(yrs::Options { skip_gc: true, ..Default::default() });
    document.get_or_insert_text("text");
    document.transact_mut().apply_update(update).unwrap();
    document
}

/// Applies updates to a new Yjs document, each in a transaction of its own: yrs's quickest way
/// to take many. Combining them first with its own call for that takes seconds on C1 and C2.
fn yjs_merge(updates: Vec<yrs::Update>) -> yrs::Doc {
    use yrs::Transact;
    let document = yrs::Doc::with_options(yrs::Options { skip_gc: true, ..Default::default() });
    document.get_or_insert_text("text");
    for update in updates {
        document.transact_mut().apply_update(update).unwrap();
    }
    document
}

/// Returns a Yjs document's text.
fn yjs_text(document: &yrs::Doc) -> String {
    use yrs::{GetString, Transact};
    document.get_or_insert_text("text").get_string(&document.transact())
}

/// Saves a Yjs document as one update of its whole state, the only form it has.
fn yjs_save(document: &mut yrs::Doc) -> Vec<u8> {
    use yrs::{ReadTxn, Transact};
    document.transact().encode_state_as_update_v2(&yrs::StateVector::default())
}

/// Imports a snapshot into a new Loro document and returns it with its text.
fn loro_import(bytes: &[u8]) -> LoroDoc {
    let document = LoroDoc::new();
    document.import(bytes).unwrap();
    document
}

/// Returns a Loro document's text.
fn loro_text(document: &LoroDoc) -> String {
    document.get_text(TEXT).to_string()
}

/// Reads the updates each writer sent into memory as ops, without applying them. Loro takes
/// remote edits as bytes or as ops in its JSON form; the latter starts from ops in memory.
fn loro_decode_batches(bytes: &[u8]) -> loro::JsonSchema {
    let reader = LoroDoc::new();
    let sent: Vec<Vec<u8>> = batches(bytes).map(<[u8]>::to_vec).collect();
    reader.import_batch(&sent).unwrap();
    reader.export_json_updates(&Default::default(), &reader.oplog_vv())
}

/// Applies ops to a new Loro document, all at once.
fn loro_apply(ops: loro::JsonSchema) -> LoroDoc {
    let document = LoroDoc::new();
    document.import_json_updates(ops).unwrap();
    document
}

/// What a system took on the two tasks.
#[derive(Clone, Copy)]
struct Measured {
    merge_ms: f64,
    /// Memory the history's ops take, in the form the system gets them.
    ops: usize,
    /// While merging: peak memory, ops included, and what the merged document holds.
    merging: Memory,
    open_ms: f64,
    /// While reopening: peak memory, and what the reopened document holds.
    opening: Memory,
    /// Size of the saved document.
    saved: usize,
    /// Whether both tasks ended with diamond-types' text.
    same: bool,
}

/// One run of merging: the milliseconds taken, then for eg-strider and diamond-types those spent
/// appending, then for eg-strider those one more edit at the end takes. And whether the text was
/// diamond-types'.
type MergeRun = ([f64; 3], bool);

/// A library's two tasks on one trace, each run once per call, so that every trace takes its
/// turn.
struct Turns<'a> {
    merge: Box<dyn FnMut() -> MergeRun + 'a>,
    open: Box<dyn FnMut() -> f64 + 'a>,
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
fn in_turns<T>(reps: u32, tasks: &mut [&mut dyn FnMut() -> T]) -> Vec<Vec<T>> {
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

/// Returns the milliseconds since `start`.
fn since(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1e3
}

/// Returns the median: the middle time, or the mean of the middle two.
fn median(mut times: Vec<f64>) -> f64 {
    times.sort_by(f64::total_cmp);
    let (below, above) = (times.len().saturating_sub(1) / 2, times.len() / 2);
    times.get(below).map_or(f64::NAN, |low| (low + times[above]) / 2.0)
}

/// Returns the median milliseconds of `reps` runs of `f`, each on what an untimed `setup`
/// makes, and the last result.
fn median_of<S, T>(reps: u32, mut setup: impl FnMut() -> S, mut f: impl FnMut(S) -> T) -> (f64, T) {
    let mut times = Vec::with_capacity(reps as usize);
    loop {
        let input = setup();
        let t = Instant::now();
        let result = f(input);
        times.push(since(t));
        if times.len() >= reps as usize {
            return (median(times), result);
        }
    }
}

/// Formats a byte count in megabytes.
fn mb(bytes: usize) -> String {
    format!("{:.2} MB", bytes as f64 / (1 << 20) as f64)
}

/// The libraries, in the order they are timed and their figures listed: each one's key in the
/// results, and its name.
const LIBRARIES: [(&str, &str); 5] = [("strider", "eg-strider"), ("dt", "diamond-types"), ("yjs", "yrs"), ("loro", "Loro"), ("automerge", "Automerge")];

/// Writes one figure per library as a JSON object, with `null` where there is none.
fn object<T: Display>(of: [Option<T>; 5]) -> String {
    let fields: Vec<String> =
        LIBRARIES.iter().zip(of).map(|((key, _), value)| format!("\"{key}\":{}", value.map_or("null".to_string(), |v| v.to_string()))).collect();
    format!("{{{}}}", fields.join(","))
}

/// Lays one figure per library out in columns, with a dash where there is none.
fn columns(of: [Option<String>; 5]) -> String {
    of.map(|value| format!("{:>13}", value.unwrap_or("-".to_string()))).join(" ")
}

/// A trace read into memory, with what each library is handed and checked against.
struct Loaded {
    name: String,
    path: String,
    events: usize,
    dt_history: History<TextOperation, usize>,
    /// What the history takes in diamond-types' form, and in eg-strider's.
    held_by_dt_history: Memory,
    history: History,
    held_by_history: Memory,
    /// diamond-types' text for the trace.
    expected: String,
    /// Whether that is the trace's own text.
    trace_text: bool,
    saved: Vec<u8>,
    dt_saved: Vec<u8>,
    /// Whether eg-strider's and diamond-types' saved documents reopen with that text.
    same: bool,
    /// The batches each writer of yrs, Loro and Automerge sent, with notes on the replay, or why
    /// there are none.
    sent: [Result<(Vec<u8>, String), &'static str>; 3],
}

/// Reads a trace and the other libraries' replays of it, replaying any not yet kept if `replay`.
fn load(path: &str, replay: bool, left_out: &[&str]) -> Loaded {
    let name = path.rsplit('/').next().unwrap().trim_end_matches(".json").to_string();
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    // What reading the trace keeps is its ops and its text.
    let (trace, read) = memory_of(|| Trace::of_json(&bytes).unwrap_or_else(|e| panic!("{path}: {e}")));
    let Trace { history: dt_history, end_content } = trace;
    let held_by_dt_history = Memory { peak: 0, kept: read.kept - end_content.capacity() };
    drop(bytes);
    let events = dt_history.events();
    let (history, held_by_history) = memory_of(|| dt_history.edits());
    // A trace numbers its agents in the order of their names, but splits an agent whose
    // edits are concurrent with its own into several, so ties between concurrent inserts
    // can fall differently from the trace's text.
    let expected = dt_merge(&dt_history).1.content().to_string();
    let trace_text = expected == end_content;
    drop(end_content);
    let saved = strider_merge(&history).save();
    let dt_saved = dt_save(&dt_append(&dt_history), &expected);
    let same = OpLog::load(saved.clone()).unwrap().checkout() == expected && dt_open(&dt_saved).1.to_string() == expected;
    let sent = [
        replayed::<compare::replicas::Yjs>(&name, "yrs", &dt_history, replay, left_out),
        replayed::<compare::replicas::Loro>(&name, "loro", &dt_history, replay, left_out),
        replayed::<compare::replicas::Automerge>(&name, "automerge", &dt_history, replay, left_out),
    ];
    Loaded { name, path: path.to_string(), events, dt_history, held_by_dt_history, history, held_by_history, expected, trace_text, saved, dt_saved, same, sent }
}

/// Sets eg-strider's two tasks on a trace up to be timed. Its merge also times its appending,
/// and one more edit at the end, merged on top of what is there.
fn strider_turns(loaded: &Loaded) -> Turns<'_> {
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
fn dt_turns(loaded: &Loaded) -> Turns<'_> {
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
fn others(loaded: &Loaded) -> [Option<(Measured, Turns<'_>)>; 3] {
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

/// Measures eg-strider beside diamond-types, yrs, Loro and Automerge on each of the paper's
/// editing traces, and checks that each ends with diamond-types' text, and whether that is the
/// trace's. The two tasks are merging a history whose ops are in memory, through each library's
/// own calls, and reopening a saved document from its bytes until it is ready for more edits.
/// The other libraries take every batch each writer's replica sent, all at once, through their
/// own calls for that. Each library is timed on every trace in turns. The paper's own Yjs and
/// Automerge files for a trace, when beside it, are opened too, to check the replays' saved
/// documents against.
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let replay = args.iter().any(|a| a == "--replay");
    let json_path = args.iter().position(|a| a == "--json").map(|i| args[i + 1].clone());
    let reps: u32 = args.iter().position(|a| a == "--reps").map_or(20, |i| args[i + 1].parse().unwrap());
    // Libraries to leave out, as yrs, loro or automerge.
    let left_out: Vec<&str> = args.windows(2).filter(|pair| pair[0] == "--without").map(|pair| pair[1].as_str()).collect();
    let paths: Vec<&String> = args
        .iter()
        .enumerate()
        .filter(|(i, a)| !a.starts_with("--") && (*i == 0 || !["--reps", "--json", "--without"].contains(&args[i - 1].as_str())))
        .map(|(_, a)| a)
        .collect();
    assert!(!paths.is_empty(), "usage: compare <trace.json>... [--replay] [--reps N] [--json OUT] [--without LIBRARY]...");

    // Every trace is read in and every library checked and weighed first. Then each library is
    // timed on every trace in turns.
    let loaded: Vec<Loaded> = paths.iter().map(|path| load(path, replay, &left_out)).collect();
    let (mut measured_others, mut tasks) = (Vec::new(), Vec::new());
    for loaded in &loaded {
        let mut turns = vec![Some(strider_turns(loaded)), Some(dt_turns(loaded))];
        let mut measured = Vec::new();
        for other in others(loaded) {
            let (other_measured, other_turns) = other.unzip();
            measured.push(other_measured);
            turns.push(other_turns);
        }
        measured_others.push(measured);
        tasks.push(turns);
    }
    let mut merge_runs: Vec<Vec<Vec<MergeRun>>> = vec![vec![Vec::new(); LIBRARIES.len()]; loaded.len()];
    let mut open_runs: Vec<Vec<Vec<f64>>> = vec![vec![Vec::new(); LIBRARIES.len()]; loaded.len()];
    for (library, (_, name)) in LIBRARIES.iter().enumerate() {
        let timed: Vec<usize> = (0..loaded.len()).filter(|&trace| tasks[trace][library].is_some()).collect();
        if timed.is_empty() {
            continue;
        }
        eprintln!("timing {name}...");
        let mut merges: Vec<&mut dyn FnMut() -> MergeRun> =
            tasks.iter_mut().filter_map(|turns| turns[library].as_mut()).map(|turns| &mut *turns.merge as _).collect();
        for (&trace, runs) in timed.iter().zip(in_turns(reps, &mut merges)) {
            merge_runs[trace][library] = runs;
        }
        drop(merges);
        let mut opens: Vec<&mut dyn FnMut() -> f64> = tasks.iter_mut().filter_map(|turns| turns[library].as_mut()).map(|turns| &mut *turns.open as _).collect();
        for (&trace, runs) in timed.iter().zip(in_turns(reps, &mut opens)) {
            open_runs[trace][library] = runs;
        }
    }
    drop(tasks);

    println!("merging a history whose ops are in memory: appending them, then merging; and then one more edit at the end, merged on top");
    println!("{:6} {:>8} {:>32} {:>42} {:>12}  result", "trace", "events", "diamond-types", "eg-strider", "one more");
    let mut all_match = true;
    let (mut merges, mut opens, mut afterwards, mut others, mut papers, mut json) = (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for (((loaded, measured), merged), opened) in loaded.iter().zip(&measured_others).zip(&merge_runs).zip(&open_runs) {
        let Loaded { name, path, events, dt_history, held_by_dt_history, history, held_by_history, expected, trace_text, saved, dt_saved, same, sent } = loaded;
        let (name, events, expected, trace_text) = (name.as_str(), *events, expected.as_str(), *trace_text);
        let (held_by_dt_history, held_by_history) = (*held_by_dt_history, *held_by_history);
        let mut same = *same && merged[..2].iter().flatten().all(|(_, same)| *same);
        let part = |runs: &[MergeRun], i: usize| median(runs.iter().map(|(ms, _)| ms[i]).collect());
        let spread = [&merged[1], &merged[0]].map(|runs| {
            let times: Vec<f64> = runs.iter().map(|(ms, _)| ms[0]).collect();
            let mean = times.iter().sum::<f64>() / times.len() as f64;
            100.0 * (times.iter().map(|t| (t - mean).powi(2)).sum::<f64>() / times.len() as f64).sqrt() / mean
        });
        let (merge_ms, append_ms, again_ms) = (part(&merged[0], 0), part(&merged[0], 1), part(&merged[0], 2));
        let (dt_ms, dt_append_ms) = (part(&merged[1], 0), part(&merged[1], 1));
        let (open_ms, dt_open_ms) = (median(opened[0].clone()), median(opened[1].clone()));
        let other = |library: usize| match (&sent[library - 2], measured[library - 2]) {
            (Ok((_, notes)), Some(measured)) => {
                Ok((Measured { merge_ms: part(&merged[library], 0), open_ms: median(opened[library].clone()), ..measured }, notes.clone()))
            }
            (Err(why), _) => Err(*why),
            (Ok(_), None) => Err("left out"),
        };
        let (yjs, loro, automerge) = (other(2), other(3), other(4));
        // A reopened document has read none of its history, which an edit made at an older
        // version then needs some of. One edit is added and merged: at the end, and at the
        // version so many events back.
        let then = [None, Some(100), Some(events / 2), Some(events - 1)].map(|back| {
            let reopen = || {
                let mut log = OpLog::load(saved.clone()).unwrap();
                log.checkout();
                log
            };
            let (ms, mut log) = median_of(reps, reopen, |mut log| {
                let typist = log.get_or_create_replica("typist");
                match back {
                    None => log.add_insert(typist, 0, "!"),
                    Some(back) => log.add_insert_at(typist, &[(log.len() - 1 - back.min(log.len() - 1)) as EventIndex], 0, "!").unwrap(),
                };
                log.checkout();
                log
            });
            // Merged from nothing, the history with that edit comes to the same text.
            let merged = log.checkout().to_string();
            log.clear_replay_cache().unwrap();
            same &= log.checkout() == merged;
            (ms, log.unread_events())
        });
        afterwards.push(format!("{name:6} {}", then.map(|(ms, _)| format!("{ms:>16.2} ms")).join(" ")));

        // Memory after the times, so that measuring it cannot disturb them. The ops each is
        // handed stay in memory throughout, so the most in use while merging is on top of them.
        let with_ops = |ops: Memory, merging: Memory| Memory { peak: ops.kept + merging.peak, kept: merging.kept };
        let strider = Measured {
            merge_ms,
            ops: held_by_history.kept,
            merging: with_ops(held_by_history, memory_of(|| strider_merge(history)).1),
            open_ms,
            opening: memory_of(|| OpLog::load(saved.clone()).unwrap()).1,
            saved: saved.len(),
            same,
        };
        let diamond = Measured {
            merge_ms: dt_ms,
            ops: held_by_dt_history.kept,
            merging: with_ops(held_by_dt_history, memory_of(|| dt_merge(dt_history)).1),
            open_ms: dt_open_ms,
            opening: memory_of(|| dt_open(dt_saved)).1,
            saved: dt_saved.len(),
            same,
        };
        all_match &= same;
        println!(
            "{name:6} {:>8} {:>7.2} + {:>6.2} = {dt_ms:>6.2} ms ±{:>3.0}% {:>7.2} + {:>6.2} = {merge_ms:>6.2} ms ±{:>3.0}% ({:>5.2}x) {again_ms:>9.2} ms  {}{}",
            events,
            dt_append_ms,
            dt_ms - dt_append_ms,
            spread[0],
            append_ms,
            merge_ms - append_ms,
            spread[1],
            dt_ms / merge_ms,
            if same { "identical" } else { "DIFFERENT" },
            if trace_text { "" } else { ", not the trace's text" }
        );

        others.push(format!(
            "{name:6} {}",
            [("yrs", &yjs), ("Loro", &loro), ("Automerge", &automerge)]
                .map(|(library, result)| match result {
                    Ok((measured, notes)) => format!("{library:>10} {notes} {:>14}", if measured.same { "same text" } else { "different text" }),
                    Err(why) => format!("{library:>10} {why:>28}"),
                })
                .join("  ")
        ));
        // The paper's own saved documents for the trace, made by Yjs and Automerge themselves, to
        // check the replays against: the time to open each beside ours, its size, and whether its
        // text is the trace's. A check, so one run of each will do.
        let beside = |suffix: &str| std::fs::read(path.replace(".json", suffix)).ok();
        let paper_yjs = beside(".yjs").filter(|_| !left_out.contains(&"yrs")).map(|file| {
            let t = Instant::now();
            let document = yjs_apply(yjs_decode(&file));
            (since(t), file.len(), yjs_text(&document) == expected)
        });
        let paper_automerge = beside(".am").filter(|_| !left_out.contains(&"automerge")).map(|file| {
            let t = Instant::now();
            let document = automerge_open(&file);
            (since(t), file.len(), automerge_text(&document) == expected)
        });
        let ours = [yjs.as_ref().ok().map(|m| m.0), automerge.as_ref().ok().map(|m| m.0)];
        papers.push(format!(
            "{name:6} {}",
            [("yrs", ours[0], paper_yjs), ("Automerge", ours[1], paper_automerge)]
                .map(|(library, ours, paper)| {
                    let (open, size) = (ours.map_or("-".to_string(), |m| format!("{:.2} ms", m.open_ms)), ours.map_or("-".to_string(), |m| mb(m.saved)));
                    let (paper_open, paper_size) = (paper.map_or("-".to_string(), |p| format!("{:.2} ms", p.0)), paper.map_or("-".to_string(), |p| mb(p.1)));
                    let text = paper.map_or("-", |p| if p.2 { "same text" } else { "different text" });
                    format!("{library:>10} {open:>11} {paper_open:>11} {size:>9} {paper_size:>9} {text:>14}")
                })
                .join("  ")
        ));

        let all = [Some(strider), Some(diamond), yjs.ok().map(|m| m.0), loro.ok().map(|m| m.0), automerge.ok().map(|m| m.0)];
        merges.push(format!(
            "{name:6} {}   {}   {}   {}",
            columns(all.map(|m| m.map(|m| format!("{:.2} ms", m.merge_ms)))),
            columns(all.map(|m| m.map(|m| mb(m.merging.peak)))),
            columns(all.map(|m| m.map(|m| mb(m.ops)))),
            columns(all.map(|m| m.map(|m| mb(m.merging.kept))))
        ));
        opens.push(format!(
            "{name:6} {}   {}   {}",
            columns(all.map(|m| m.map(|m| format!("{:.2} ms", m.open_ms)))),
            columns(all.map(|m| m.map(|m| mb(m.opening.kept)))),
            columns(all.map(|m| m.map(|m| mb(m.saved))))
        ));
        json.push(format!(
            "{{\"trace\":\"{name}\",\"events\":{events},\"reps\":{reps},\"trace_text\":{trace_text},\"same\":{},\"merge_ms\":{},\"open_ms\":{},\"merge_ops\":{},\"merge_peak\":{},\"merge_kept\":{},\"open_peak\":{},\"open_kept\":{},\"saved\":{},\"parts_ms\":{{\"strider_append\":{append_ms:.4},\"strider_again\":{again_ms:.4},\"dt_append\":{dt_append_ms:.4},\"strider_then\":[{}]}}}}",
            object(all.map(|m| m.map(|m| m.same))),
            object(all.map(|m| m.map(|m| format!("{:.4}", m.merge_ms)))),
            object(all.map(|m| m.map(|m| format!("{:.4}", m.open_ms)))),
            object(all.map(|m| m.map(|m| m.ops))),
            object(all.map(|m| m.map(|m| m.merging.peak))),
            object(all.map(|m| m.map(|m| m.merging.kept))),
            object(all.map(|m| m.map(|m| m.opening.peak))),
            object(all.map(|m| m.map(|m| m.opening.kept))),
            object(all.map(|m| m.map(|m| m.saved))),
            then.map(|(ms, _)| format!("{ms:.4}")).join(","),
        ));
    }

    let headings = || columns(LIBRARIES.map(|(_, name)| Some(name.to_string())));
    println!(
        "\nmerging a history whose ops are in memory: the time, the most memory in use, how much of that the ops are, and what the document holds once done"
    );
    println!("{:6} {}   {}   {}   {}", "trace", headings(), headings(), headings(), headings());
    for line in merges {
        println!("{line}");
    }
    println!("\nreopening a saved document from its bytes: the time, what the document then holds, and the size of the file");
    println!("{:6} {}   {}   {}", "trace", headings(), headings(), headings());
    for line in opens {
        println!("{line}");
    }
    println!("\neg-strider, reopened, adding one edit and merging it: an edit at the end, and one made at the version so far back");
    println!("{:6} {:>19} {:>19} {:>19} {:>19}", "trace", "at the end", "100 events back", "halfway back", "at the start");
    for line in afterwards {
        println!("{line}");
    }
    println!("\nthe other libraries' replays of each history: peers, edits that had to be moved, and whether the text came out the same");
    for line in others {
        println!("{line}");
    }
    println!("\nthe paper's own Yjs and Automerge files beside the saved documents of these replays: the time to open each, its size, and whether the paper's file ends with diamond-types' text");
    println!(
        "{:6} {}",
        "trace",
        ["yrs", "Automerge"]
            .map(|library| format!("{library:>10} {:>11} {:>11} {:>9} {:>9} {:>14}", "ours", "paper's", "ours", "paper's", "paper's text"))
            .join("  ")
    );
    for line in papers {
        println!("{line}");
    }
    if let Some(path) = json_path {
        std::fs::write(&path, format!("[\n{}\n]\n", json.join(",\n"))).unwrap();
    }
    if !all_match {
        std::process::exit(1);
    }
}
