//! Replays each trace from an empty document with diamond-types and with eg-strider, checks
//! that the documents are identical, and prints the times and the memory used side by side.

use diamond_types::list::ListOpLog;
use loro::{ExportMode, LoroDoc};
use compare::replicas::TEXT;
use std::alloc::{GlobalAlloc, Layout, System};
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

/// What running something did to the memory in use, in bytes.
#[derive(Clone, Copy)]
struct Memory {
    /// The most in use at any moment, beyond what was in use at the start.
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

/// Loads the history as Loro updates and as a Loro snapshot, with the notes on its replay.
fn in_loro(path: &str, dt: &ListOpLog, replay: bool) -> Result<(Vec<u8>, Vec<u8>, String), &'static str> {
    let (updates, notes) = replayed::<compare::replicas::Loro>(path, "loro", dt, replay)?;
    let doc = LoroDoc::new();
    doc.import(&updates).unwrap();
    let snapshot = doc.export(ExportMode::snapshot()).unwrap();
    Ok((updates, snapshot, notes))
}

/// How long replaying a history into another library may take before it is given up on.
const LIMIT: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// Loads the history in another library's format, with how many peers it took and how many
/// edits had to be moved. Replayed into the library on first use and kept beside the trace.
fn replayed<R: compare::replicas::Replica>(path: &str, kind: &str, dt: &ListOpLog, replay: bool) -> Result<(Vec<u8>, String), &'static str> {
    let file = |suffix: &str| format!("{path}.{kind}{suffix}");
    if let (Ok(bytes), Ok(notes)) = (std::fs::read(file("")), std::fs::read_to_string(file("-notes"))) {
        return Ok((bytes, notes));
    }
    if std::path::Path::new(&file("-unfinished")).exists() {
        return Err("did not finish");
    }
    if !replay {
        return Err("not replayed yet: pass --replay");
    }
    eprintln!("replaying {path} into {kind} (once)...");
    let Some(replayed) = compare::replicas::replay::<R>(dt, LIMIT) else {
        std::fs::write(file("-unfinished"), "").unwrap();
        return Err("did not finish");
    };
    let notes = format!("{:>6} {:>6}", replayed.peers, replayed.moved);
    std::fs::write(file(""), &replayed.saved).unwrap();
    std::fs::write(file("-notes"), &notes).unwrap();
    Ok((replayed.saved, notes))
}

/// Loads an Automerge document saved as `bytes` and returns it with its text.
fn automerge_load(bytes: &[u8]) -> (automerge::AutoCommit, String) {
    use automerge::ReadDoc;
    let doc = automerge::AutoCommit::load(bytes).unwrap();
    let (_, text) = doc.get(automerge::ROOT, "text").unwrap().unwrap();
    let string = doc.text(&text).unwrap();
    (doc, string)
}

/// Applies a history to a new Yjs document (its Rust port, yrs) and returns it with its text.
fn yjs_load(bytes: &[u8]) -> (yrs::Doc, String) {
    use yrs::updates::decoder::Decode;
    use yrs::{GetString, Transact};
    let doc = yrs::Doc::new();
    let text = doc.get_or_insert_text("text");
    doc.transact_mut().apply_update(yrs::Update::decode_v2(bytes).unwrap());
    let string = text.get_string(&doc.transact());
    (doc, string)
}

/// Reads a saved Yjs history into memory, without applying it to a document.
fn yjs_decode(bytes: &[u8]) -> yrs::Update {
    use yrs::updates::decoder::Decode;
    yrs::Update::decode_v2(bytes).unwrap()
}

/// Loads a history into another library: the time and memory it takes, and a line saying so.
/// The memory kept is the document the library is left holding.
fn measure<D>(replayed: Result<(Vec<u8>, String), &'static str>, load: fn(&[u8]) -> (D, String), reps: u32, expected: &str) -> (Option<(f64, Memory)>, String) {
    match replayed {
        Ok((bytes, notes)) => {
            let memory = memory_of(|| load(&bytes).0).1;
            let (ms, text) = median_of(reps, || load(&bytes).1);
            let same = if text == expected { "same text" } else { "different text" };
            (Some((ms, memory)), format!("{notes} {ms:>10.2} ms {:>12} {:>12} {same:>15}", mb(memory.peak), mb(memory.kept)))
        }
        Err(why) => (None, format!("{why:>68}")),
    }
}

/// Returns the median of some times: the middle one, or the mean of the middle two.
fn median(mut times: Vec<f64>) -> f64 {
    times.sort_by(f64::total_cmp);
    let (below, above) = (times.len().saturating_sub(1) / 2, times.len() / 2);
    times.get(below).map_or(f64::NAN, |low| (low + times[above]) / 2.0)
}

/// Returns the median time in milliseconds of up to `reps` runs, fewer once they have taken
/// three seconds, and the last result.
fn median_of<T>(reps: u32, mut f: impl FnMut() -> T) -> (f64, T) {
    let (mut times, mut spent) = (Vec::with_capacity(reps as usize), 0.0);
    loop {
        let t = Instant::now();
        let result = f();
        let ms = t.elapsed().as_secs_f64() * 1e3;
        times.push(ms);
        spent += ms;
        if times.len() >= reps as usize || spent > 3000.0 {
            return (median(times), result);
        }
    }
}

/// Imports `bytes` into a new Loro document and returns it with its text.
fn loro_import(bytes: &[u8]) -> (LoroDoc, String) {
    let doc = LoroDoc::new();
    doc.import(bytes).unwrap();
    let string = doc.get_text(TEXT).to_string();
    (doc, string)
}

/// Formats a byte count in megabytes.
fn mb(bytes: usize) -> String {
    format!("{:.2} MB", bytes as f64 / (1 << 20) as f64)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let replay = args.iter().any(|a| a == "--replay");
    let json_path = args.iter().position(|a| a == "--json").map(|i| args[i + 1].clone());
    let reps: u32 = args.iter().position(|a| a == "--reps").map_or(20, |i| args[i + 1].parse().unwrap());
    let paths: Vec<&String> = args
        .iter()
        .enumerate()
        .filter(|(i, a)| !a.starts_with("--") && (*i == 0 || !["--reps", "--json"].contains(&args[i - 1].as_str())))
        .map(|(_, a)| a)
        .collect();
    assert!(!paths.is_empty(), "usage: compare <trace.dt>... [--reps N]");

    println!("{:6} {:>8} {:>14} {:>20} {:>20}  result", "trace", "events", "diamond-types", "first checkout", "later checkouts");
    let mut all_match = true;
    let mut memory = Vec::new();
    let mut from_disks = Vec::new();
    let mut loads = Vec::new();
    let mut loro = Vec::new();
    let mut others = Vec::new();
    let mut json = Vec::new();
    for path in paths {
        let name = path.rsplit('/').next().unwrap().trim_end_matches(".dt");
        let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{path}: {e}"));
        let (dt, dt_log) = memory_of(|| ListOpLog::load_from(&bytes).unwrap());
        // eg-strider's own file for the history, which everything it is measured on starts from.
        let saved = compare::file::save(&dt);
        let (mut log, strider_log) = memory_of(|| compare::file::load(&saved));

        let disk = bytes.len();
        let in_loro = in_loro(path, &dt, replay);

        // Times first, so that measuring memory cannot disturb them.
        let mut times: [Vec<f64>; 7] = std::array::from_fn(|_| Vec::with_capacity(reps as usize));
        let [dt_ms, first_ms, later_ms, import_ms, loro_load_ms, merge_ms, snapshot_ms] = &mut times;
        let (mut same, mut loro_same) = (true, true);
        for _ in 0..reps {
            let t = Instant::now();
            let expected = dt.checkout_tip().content().to_string();
            dt_ms.push(t.elapsed().as_secs_f64() * 1e3);

            log.forget();
            let t = Instant::now();
            let first = log.checkout();
            first_ms.push(t.elapsed().as_secs_f64() * 1e3);

            let t = Instant::now();
            let later = log.checkout();
            later_ms.push(t.elapsed().as_secs_f64() * 1e3);
            same &= expected == first && expected == later;

            let Ok((updates, snapshot, _)) = &in_loro else { continue };
            let t = Instant::now();
            let imported = loro_import(updates).1;
            import_ms.push(t.elapsed().as_secs_f64() * 1e3);

            // A checkout: the updates are read in while the document is detached, which
            // leaves its state untouched, and it is brought up to date on attaching.
            let doc = LoroDoc::new();
            doc.detach();
            let t = Instant::now();
            doc.import(updates).unwrap();
            loro_load_ms.push(t.elapsed().as_secs_f64() * 1e3);
            let t = Instant::now();
            doc.attach();
            let merged = doc.get_text(TEXT).to_string();
            merge_ms.push(t.elapsed().as_secs_f64() * 1e3);

            let t = Instant::now();
            let loaded = loro_import(snapshot).1;
            snapshot_ms.push(t.elapsed().as_secs_f64() * 1e3);
            loro_same &= expected == imported && expected == merged && expected == loaded;
        }
        let [dt_ms, first_ms, later_ms, import_ms, loro_load_ms, merge_ms, snapshot_ms] = times.map(median);

        // Loading alone, each from its own file.
        let dt_load_ms = median_of(reps, || ListOpLog::load_from(&bytes).unwrap()).0;
        let load_ms = median_of(reps, || compare::file::load(&saved)).0;

        // Memory. A closure's result counts as kept only if it is returned, so here the text
        // is measured and dropped.
        let (text_len, dt_checkout) = memory_of(|| dt.checkout_tip().content().to_string().len());
        log.forget();
        let (text, with_text) = memory_of(|| log.checkout());
        let first_checkout = Memory { peak: with_text.peak, kept: with_text.kept - text.capacity() };
        drop(text);
        let (_, later_checkout) = memory_of(|| log.checkout().len());
        // As the paper measures it: from the history on disk to a document that is ready to
        // merge again, with the history counted.
        let dt_from_disk = memory_of(|| {
            let history = ListOpLog::load_from(&bytes).unwrap();
            let document = history.checkout_tip();
            (history, document)
        })
        .1;
        let from_disk = memory_of(|| {
            let mut history = compare::file::load(&saved);
            let document = history.checkout();
            (history, document)
        })
        .1;
        from_disks.push(format!(
            "{name:6} {:>10} {:>10} {:>12} {:>10} {:>12}",
            mb(disk),
            mb(dt_from_disk.peak),
            mb(dt_from_disk.kept),
            mb(from_disk.peak),
            mb(from_disk.kept)
        ));
        let loro_memory = in_loro.as_ref().ok().map(|(updates, snapshot, _)| {
            (memory_of(|| loro_import(updates).0).1, memory_of(|| loro_import(snapshot).0).1.peak)
        });
        memory.push(format!(
            "{name:6} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10}",
            mb(text_len),
            mb(dt_log.kept),
            mb(strider_log.kept),
            mb(dt_checkout.peak),
            mb(first_checkout.peak),
            mb(later_checkout.peak),
            mb(first_checkout.kept),
        ));
        loro.push(match (&in_loro, loro_memory) {
            (Ok((_, _, notes)), Some((updates, snapshot))) => format!(
                "{name:6} {notes} {import_ms:>10.2} ms {merge_ms:>10.2} ms {snapshot_ms:>10.2} ms {:>12} {:>12} {:>12}  {}",
                mb(updates.peak),
                mb(updates.kept),
                mb(snapshot),
                if loro_same { "same text" } else { "different text" },
            ),
            (Err(why), _) => format!("{name:6} {why}"),
            (Ok(_), None) => unreachable!(),
        });

        // Automerge and Yjs, on the same histories replayed into each.
        let expected = dt.checkout_tip().content().to_string();
        let (automerge, automerge_row) = measure(replayed::<compare::replicas::Automerge>(path, "automerge", &dt, replay), automerge_load, reps, &expected);
        let in_yjs = replayed::<compare::replicas::Yjs>(path, "yrs", &dt, replay);
        let yjs_load_ms = in_yjs.as_ref().ok().map(|(bytes, _)| median_of(reps, || yjs_decode(bytes)).0);
        let (yjs, yjs_row) = measure(in_yjs, yjs_load, reps, &expected);
        others.push(format!("{name:6} {automerge_row}   {yjs_row}"));

        let ms = |v: Option<f64>| v.map_or("null".to_string(), |v| format!("{v:.4}"));
        let bytes = |v: Option<usize>| v.map_or("null".to_string(), |v| v.to_string());
        let loro_ms = |v: f64| in_loro.as_ref().ok().map(|_| v);
        // Yjs and Loro are timed from bytes to document, and what reading the bytes in leaves
        // of that is their merge. Automerge has no such step apart, so all of its time is.
        let load_of = [Some(load_ms), Some(dt_load_ms), yjs_load_ms, loro_ms(loro_load_ms)];
        let merge_of = [
            Some(first_ms),
            Some(dt_ms),
            yjs.zip(yjs_load_ms).map(|(whole, load)| whole.0 - load),
            loro_ms(import_ms - loro_load_ms),
            automerge.map(|m| m.0),
        ];
        let shown = |v: Option<f64>| v.map_or("-".to_string(), |v| format!("{v:.2} ms"));
        loads.push(format!(
            "{name:6} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10}",
            mb(disk),
            mb(saved.len()),
            shown(load_of[1]),
            shown(load_of[0]),
            shown(load_of[2]),
            shown(load_of[3]),
            shown(merge_of[1]),
            shown(merge_of[0]),
            shown(merge_of[2]),
            shown(merge_of[3]),
            shown(merge_of[4]),
        ));
        json.push(format!(
            "{{\"trace\":\"{name}\",\"events\":{},\"disk\":{disk},\"load_ms\":{{\"strider_first\":{},\"egwalker\":{},\"yjs\":{},\"loro\":{}}},\"merge_ms\":{{\"strider_first\":{},\"egwalker\":{},\"yjs\":{},\"loro\":{},\"automerge\":{}}},\"ms\":{{\"strider_first\":{},\"strider_later\":{},\"egwalker\":{},\"loro\":{},\"loro_checkout\":{},\"loro_snapshot\":{},\"automerge\":{},\"yjs\":{}}},\"peak\":{{\"strider_first\":{},\"egwalker\":{},\"loro\":{},\"loro_snapshot\":{},\"automerge\":{},\"yjs\":{}}},\"retained\":{{\"strider_first\":{},\"egwalker\":{},\"loro\":{},\"automerge\":{},\"yjs\":{}}},\"beyond_history\":{{\"strider_first\":{},\"strider_later\":{},\"strider_kept\":{},\"egwalker\":{}}}}}",
            dt.len(),
            ms(load_of[0]), ms(load_of[1]), ms(load_of[2]), ms(load_of[3]),
            ms(merge_of[0]), ms(merge_of[1]), ms(merge_of[2]), ms(merge_of[3]), ms(merge_of[4]),
            ms(Some(first_ms)), ms(Some(later_ms)), ms(Some(dt_ms)), ms(loro_ms(import_ms)), ms(loro_ms(merge_ms)), ms(loro_ms(snapshot_ms)), ms(automerge.map(|m| m.0)), ms(yjs.map(|m| m.0)),
            from_disk.peak, dt_from_disk.peak,
            bytes(loro_memory.map(|m| m.0.peak)), bytes(loro_memory.map(|m| m.1)), bytes(automerge.map(|m| m.1.peak)), bytes(yjs.map(|m| m.1.peak)),
            from_disk.kept, dt_from_disk.kept,
            bytes(loro_memory.map(|m| m.0.kept)), bytes(automerge.map(|m| m.1.kept)), bytes(yjs.map(|m| m.1.kept)),
            first_checkout.peak, later_checkout.peak, first_checkout.kept, dt_checkout.peak,
        ));

        all_match &= same;
        println!(
            "{name:6} {:>8} {dt_ms:>11.2} ms {first_ms:>9.2} ms ({:>5.2}x) {later_ms:>9.2} ms ({:>5.2}x)  {}",
            dt.len(),
            dt_ms / first_ms,
            dt_ms / later_ms,
            if same { "identical" } else { "DIFFERENT" }
        );
    }

    println!("\nloading and merging apart: each history's file, as diamond-types stores it and as `compare::file` does; the time to read a history into memory; and the time to merge it from there");
    println!(
        "{:6} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10}",
        "trace", "dt file", "file", "dt load", "load", "yjs load", "loro load", "dt merge", "merge", "yjs merge", "loro merge", "automerge"
    );
    for line in loads {
        println!("{line}");
    }

    println!("\nmemory: the log at rest, the most in use during a checkout beyond the log, and what a checkout keeps for the next");
    println!("{:6} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10}", "trace", "text", "dt log", "log", "dt peak", "first", "later", "kept");
    for line in memory {
        println!("{line}");
    }

    println!("\nmemory as the paper measures it: the history on disk, then the most in use while loading and merging it, and what is retained ready to merge again");
    println!("{:6} {:>10} {:>10} {:>12} {:>10} {:>12}", "trace", "on disk", "dt peak", "dt retained", "peak", "retained");
    for line in from_disks {
        println!("{line}");
    }

    println!("\nLoro on the same histories: time to import its updates, to check out from its log, and to import a snapshot; then the most memory in use while importing its updates, what the document then holds, and the most in use while importing a snapshot");
    println!("{:6} {:>6} {:>6} {:>13} {:>13} {:>13} {:>12} {:>12} {:>12}", "trace", "peers", "moved", "updates", "checkout", "snapshot", "updates", "document", "snapshot");
    for line in loro {
        println!("{line}");
    }
    println!("\nAutomerge, then Yjs (yrs), on the same histories: peers, edits moved, time to load, most memory in use, what the document then holds, and whether the text is the same");
    for line in others {
        println!("{line}");
    }
    if let Some(path) = json_path {
        std::fs::write(&path, format!("[\n{}\n]\n", json.join(",\n"))).unwrap();
    }
    if !all_match {
        std::process::exit(1);
    }
}
