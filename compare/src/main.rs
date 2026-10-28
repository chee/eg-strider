mod measure;

use compare::replicas::TEXT;
use compare::{History, Trace};
use diamond_types::list::encoding::EncodeOptions;
use diamond_types::list::operation::TextOperation;
use diamond_types::list::{ListBranch, ListOpLog};
use eg_strider::{EventIndex, OpLog};
use jumprope::JumpRope;
use loro::{ExportMode, LoroDoc};
use measure::{in_turns, median, median_of, prepare, since, Document, Measured, Turns};
use std::fmt::Display;
use std::time::Instant;

/// How long replaying a history into another library may take before it is given up.
const LIMIT: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// Where replays into other libraries are kept, as the batches each writer sent.
const REPLAYED: &str = "traces/batches";

/// A history's replay into another library, as the batches each writer's replica sent and notes on
/// peers and moved edits, replayed on first use and kept in [`REPLAYED`]; or why there is none.
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

/// Reads a saved eg-strider document's whole history and merges it from nothing, ignoring the
/// text saved with it.
fn strider_replay(bytes: Vec<u8>) -> OpLog {
    let mut log = OpLog::load(bytes).unwrap();
    log.clear_replay_cache().unwrap();
    log.checkout();
    log
}

/// Reopens a saved eg-strider document, which reads its text and leaves its history for later.
fn strider_open(bytes: Vec<u8>) -> OpLog {
    let mut log = OpLog::load(bytes).unwrap();
    log.checkout();
    log
}

impl Document for OpLog {
    fn text(&mut self) -> String {
        self.checkout().to_string()
    }
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

/// Reads a diamond-types history file and merges it, as the paper's benchmark does.
fn dt_merge(bytes: Vec<u8>) -> (ListOpLog, ListBranch) {
    let oplog = ListOpLog::load_from(&bytes).unwrap();
    let branch = oplog.checkout_tip();
    (oplog, branch)
}

impl Document for (ListOpLog, ListBranch) {
    fn text(&mut self) -> String {
        self.1.content().to_string()
    }
}

/// A saved diamond-types document: the lengths, its history file, and its text compressed as
/// eg-strider's is, which the paper keeps beside the history so that opening needs no merge.
fn dt_save(oplog: &ListOpLog, text: &str) -> Vec<u8> {
    let file = oplog.encode(&EncodeOptions::default());
    let lens = [file.len() as u64, text.len() as u64].map(u64::to_le_bytes).concat();
    [&lens[..], &file, &zstd::bulk::compress(text.as_bytes(), 3).unwrap()].concat()
}

/// Reads a saved diamond-types document: its history, and its text as a rope.
fn dt_open(saved: Vec<u8>) -> (ListOpLog, JumpRope) {
    let (lens, rest) = saved.split_first_chunk::<16>().unwrap();
    let [file_len, text_len] = [0, 8].map(|at| u64::from_le_bytes(lens[at..at + 8].try_into().unwrap()) as usize);
    let (file, text) = rest.split_at(file_len);
    let text = String::from_utf8(zstd::bulk::decompress(text, text_len).unwrap()).unwrap();
    (ListOpLog::load_from(file).unwrap(), JumpRope::from(text.as_str()))
}

impl Document for (ListOpLog, JumpRope) {
    fn text(&mut self) -> String {
        self.1.to_string()
    }
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

/// Applies the changes each writer sent to a new Automerge document, all at once.
fn automerge_replayed(sent: &[u8]) -> automerge::AutoCommit {
    let changes: Vec<automerge::Change> = batches(sent).map(|raw| automerge::Change::try_from(raw).unwrap()).collect();
    let mut document = automerge::AutoCommit::new();
    document.apply_changes(changes).unwrap();
    document
}

/// Opens a saved Automerge document, which holds every change.
fn automerge_open(saved: &[u8]) -> automerge::AutoCommit {
    automerge::AutoCommit::load(saved).unwrap()
}

/// Converts a saved Automerge document into a bundle of the hashless branch, the format its `next`
/// documents save in and load fastest from.
fn automerge_bundle(saved: &[u8]) -> Vec<u8> {
    automerge_hashless::next::Automerge::load(saved).unwrap().save()
}

/// Opens a bundle as a `next` document of the hashless branch, which keeps only the change hashes
/// it needs.
fn automerge_next_open(saved: &[u8]) -> automerge_hashless::next::Automerge {
    automerge_hashless::next::Automerge::load(saved).unwrap()
}

impl Document for automerge_hashless::next::Automerge {
    fn text(&mut self) -> String {
        use automerge_hashless::next::ReadDoc;
        let (_, text) = self.get(automerge_hashless::ROOT, TEXT).unwrap().unwrap();
        ReadDoc::text(self, &text).unwrap()
    }
}

impl Document for automerge::AutoCommit {
    fn text(&mut self) -> String {
        use automerge::ReadDoc;
        let (_, text) = self.get(automerge::ROOT, TEXT).unwrap().unwrap();
        ReadDoc::text(self, &text).unwrap()
    }
}

/// A new Yjs document, with Yjs's default options.
fn yjs_new() -> yrs::Doc {
    let document = yrs::Doc::new();
    document.get_or_insert_text(TEXT);
    document
}

/// Applies the updates each writer sent to a new Yjs document, each in a transaction of its own.
fn yjs_replayed(sent: &[u8]) -> yrs::Doc {
    use yrs::updates::decoder::Decode;
    use yrs::Transact;
    let document = yjs_new();
    for update in batches(sent) {
        document.transact_mut().apply_update(yrs::Update::decode_v1(update).unwrap()).unwrap();
    }
    document
}

/// Saves a Yjs document as one update of its whole state, the form Yjs stores a document in, in
/// the second version of its encoding, as the paper's files are.
fn yjs_save(document: &yrs::Doc) -> Vec<u8> {
    use yrs::{ReadTxn, Transact};
    document.transact().encode_state_as_update_v2(&yrs::StateVector::default())
}

/// Applies one update of a whole state to a new Yjs document.
fn yjs_open(saved: &[u8]) -> yrs::Doc {
    use yrs::updates::decoder::Decode;
    use yrs::Transact;
    let document = yjs_new();
    document.transact_mut().apply_update(yrs::Update::decode_v2(saved).unwrap()).unwrap();
    document
}

impl Document for yrs::Doc {
    fn text(&mut self) -> String {
        use yrs::{GetString, Transact};
        self.get_or_insert_text(TEXT).get_string(&self.transact())
    }
}

/// Imports the updates each writer sent into a new Loro document, all at once.
fn loro_replayed(sent: &[u8]) -> LoroDoc {
    let document = LoroDoc::new();
    let sent: Vec<Vec<u8>> = batches(sent).map(<[u8]>::to_vec).collect();
    document.import_batch(&sent).unwrap();
    document
}

/// Imports updates or a snapshot into a new Loro document.
fn loro_import(bytes: &[u8]) -> LoroDoc {
    let document = LoroDoc::new();
    document.import(bytes).unwrap();
    document
}

impl Document for LoroDoc {
    fn text(&mut self) -> String {
        self.get_text(TEXT).to_string()
    }
}

fn mb(bytes: usize) -> String {
    format!("{:.2} MB", bytes as f64 / (1 << 20) as f64)
}

/// The libraries, in the order they are timed and their figures listed: each one's key in the
/// results, and its name.
const LIBRARIES: [(&str, &str); 6] =
    [("strider", "eg-strider"), ("dt", "diamond-types"), ("yjs", "yrs"), ("loro", "Loro"), ("automerge", "Automerge"), ("automerge_next", "Automerge next")];

/// Writes one figure per library as a JSON object, with `null` where there is none.
fn object<T: Display>(of: [Option<T>; LIBRARIES.len()]) -> String {
    let fields: Vec<String> =
        LIBRARIES.iter().zip(of).map(|((key, _), value)| format!("\"{key}\":{}", value.map_or("null".to_string(), |v| v.to_string()))).collect();
    format!("{{{}}}", fields.join(","))
}

/// Lays one figure per library out in columns, with a dash where there is none.
fn columns(of: [Option<String>; LIBRARIES.len()]) -> String {
    of.map(|value| format!("{:>13}", value.unwrap_or("-".to_string()))).join(" ")
}

/// A trace read into memory, with what each library is handed and checked against.
struct Loaded {
    name: String,
    events: usize,
    dt_history: History<TextOperation, usize>,
    /// diamond-types' text for the trace.
    expected: String,
    /// Whether that is the trace's own text.
    trace_text: bool,
    /// eg-strider's saved document.
    saved: Vec<u8>,
    /// The batches each writer of yrs, Loro and Automerge sent, with notes on the replay, or why
    /// there are none.
    sent: [Result<(Vec<u8>, String), &'static str>; 3],
    /// The paper's own Yjs and Automerge files for the trace, made by those libraries themselves,
    /// when beside it and not left out.
    paper: [Option<Vec<u8>>; 2],
}

/// Reads a trace and the other libraries' replays of it, replaying any not yet kept if `replay`.
/// The history is read from the paper's diamond-types file beside the trace when there is one,
/// which keeps the agents' names: the JSON numbers agents instead, splitting one whose edits are
/// concurrent with its own into several, so that ties between concurrent inserts can fall
/// differently from the trace's text.
fn load(path: &str, replay: bool, left_out: &[&str]) -> Loaded {
    let name = path.rsplit('/').next().unwrap().trim_end_matches(".json").to_string();
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let Trace { history: dt_history, end_content } = Trace::of_json(&bytes).unwrap_or_else(|e| panic!("{path}: {e}"));
    drop(bytes);
    let dt_history = match std::fs::read(path.replace(".json", ".dt")) {
        Ok(file) => History::of(&ListOpLog::load_from(&file).unwrap_or_else(|e| panic!("{path}: {e:?}"))),
        Err(_) => dt_history,
    };
    let events = dt_history.events();
    let expected = dt_append(&dt_history).checkout_tip().content().to_string();
    let trace_text = expected == end_content;
    drop(end_content);
    let saved = strider_merge(&dt_history.edits()).save();
    let sent = [
        replayed::<compare::replicas::Yjs>(&name, "yrs", &dt_history, replay, left_out),
        replayed::<compare::replicas::Loro>(&name, "loro", &dt_history, replay, left_out),
        replayed::<compare::replicas::Automerge>(&name, "automerge", &dt_history, replay, left_out),
    ];
    let beside = |suffix: &str, library: &str| std::fs::read(path.replace(".json", suffix)).ok().filter(|_| !left_out.contains(&library));
    let paper = [beside(".yjs", "yrs"), beside(".am", "automerge")];
    Loaded { name, events, dt_history, expected, trace_text, saved, sent, paper }
}

/// The Yjs and Automerge documents a trace is measured on: the paper's own files where there are
/// any, made by those libraries themselves, and otherwise the saved documents of the replays.
fn crdt_documents(loaded: &Loaded) -> [Option<Vec<u8>>; 2] {
    let [yjs, _, automerge] = &loaded.sent;
    let [paper_yjs, paper_automerge] = loaded.paper.clone();
    [
        paper_yjs.or_else(|| yjs.as_ref().ok().map(|(sent, _)| yjs_save(&yjs_replayed(sent)))),
        paper_automerge.or_else(|| automerge.as_ref().ok().map(|(sent, _)| automerge_replayed(sent).save())),
    ]
}

/// Checks and weighs every library on a trace, and sets their tasks up to be timed. Each merges
/// from its own encoding of the whole history and reopens its own saved document. Yjs and
/// Automerge are CRDTs, whose saved document is their history, so both tasks start from the same
/// bytes for them. Automerge's hashless branch is handed the Automerge document as a bundle, the
/// format its `next` documents save in.
fn libraries(loaded: &Loaded) -> [Option<(Measured, Turns<'_>)>; LIBRARIES.len()] {
    let expected = loaded.expected.as_str();
    let loro = &loaded.sent[1];
    let [yjs, automerge] = crdt_documents(loaded);
    let oplog = dt_append(&loaded.dt_history);
    [
        Some(prepare(expected, loaded.saved.clone(), strider_replay, loaded.saved.clone(), strider_open)),
        Some(prepare(expected, oplog.encode(&EncodeOptions::default()), dt_merge, dt_save(&oplog, expected), dt_open)),
        yjs.map(|saved| prepare(expected, saved.clone(), |bytes| yjs_open(&bytes), saved, |bytes| yjs_open(&bytes))),
        loro.as_ref().ok().map(|(sent, _)| {
            let document = loro_replayed(sent);
            let (history, saved) = (document.export(ExportMode::all_updates()).unwrap(), document.export(ExportMode::snapshot()).unwrap());
            prepare(expected, history, |bytes| loro_import(&bytes), saved, |bytes| loro_import(&bytes))
        }),
        automerge.as_ref().map(|saved| prepare(expected, saved.clone(), |bytes| automerge_open(&bytes), saved.clone(), |bytes| automerge_open(&bytes))),
        automerge.map(|saved| {
            let bundle = automerge_bundle(&saved);
            prepare(expected, bundle.clone(), |bytes| automerge_next_open(&bytes), bundle, |bytes| automerge_next_open(&bytes))
        }),
    ]
}

/// Measures eg-strider beside diamond-types, yrs, Loro, Automerge and Automerge's hashless branch on
/// the paper's editing traces, merging a history from each library's own encoding of it and
/// reopening a saved document, and checks each result against diamond-types' text.
///
/// The paper's own Yjs and Automerge files for a trace, when beside it, are opened to check the
/// replays' saved documents against.
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
    let (mut measured, mut tasks): (Vec<_>, Vec<_>) =
        loaded.iter().map(|loaded| libraries(loaded).into_iter().map(Option::unzip).unzip::<_, _, Vec<_>, Vec<_>>()).unzip();
    let mut merge_runs: Vec<Vec<Vec<f64>>> = vec![vec![Vec::new(); LIBRARIES.len()]; loaded.len()];
    let mut open_runs: Vec<Vec<Vec<f64>>> = vec![vec![Vec::new(); LIBRARIES.len()]; loaded.len()];
    for (library, (_, name)) in LIBRARIES.iter().enumerate() {
        let timed: Vec<usize> = (0..loaded.len()).filter(|&trace| tasks[trace][library].is_some()).collect();
        if timed.is_empty() {
            continue;
        }
        eprintln!("timing {name}...");
        let mut merges: Vec<&mut dyn FnMut() -> f64> =
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
    for ((measured, merged), opened) in measured.iter_mut().zip(&merge_runs).zip(&open_runs) {
        for ((library, merged), opened) in measured.iter_mut().zip(merged).zip(opened) {
            if let Some(library) = library {
                (library.merge_ms, library.open_ms) = (median(merged.clone()), median(opened.clone()));
            }
        }
    }

    let mut all_match = true;
    let (mut merges, mut opens, mut afterwards, mut others, mut papers, mut json) = (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for ((loaded, measured), merged) in loaded.iter().zip(&measured).zip(&merge_runs) {
        let Loaded { name, events, expected, trace_text, saved, sent, .. } = loaded;
        let (name, events, expected, trace_text) = (name.as_str(), *events, expected.as_str(), *trace_text);
        let mut all: [Option<Measured>; LIBRARIES.len()] = measured.clone().try_into().ok().unwrap();
        // A reopened document has read none of its history, which an edit made at an older
        // version then needs some of. One edit is added and merged: at the end, and at the
        // version so many events back.
        let mut same = true;
        let then = [None, Some(100), Some(events / 2), Some(events - 1)].map(|back| {
            let (ms, mut log) = median_of(
                reps,
                || strider_open(saved.clone()),
                |mut log| {
                    let typist = log.get_or_create_replica("typist");
                    match back {
                        None => log.add_insert(typist, 0, "!"),
                        Some(back) => log.add_insert_at(typist, &[(log.len() - 1 - back.min(log.len() - 1)) as EventIndex], 0, "!").unwrap(),
                    };
                    log.checkout();
                    log
                },
            );
            // Merged from nothing, the history with that edit comes to the same text.
            let merged = log.checkout().to_string();
            log.clear_replay_cache().unwrap();
            same &= log.checkout() == merged;
            (ms, log.unread_events())
        });
        if let Some(strider) = &mut all[0] {
            strider.same &= same;
        }
        all_match &= all[..2].iter().all(|library| library.is_some_and(|library| library.same));
        afterwards.push(format!("{name:6} {}", then.map(|(ms, _)| format!("{ms:>16.2} ms")).join(" ")));

        let spread = merged.iter().map(|times| {
            let mean = times.iter().sum::<f64>() / times.len() as f64;
            100.0 * (times.iter().map(|t| (t - mean).powi(2)).sum::<f64>() / times.len() as f64).sqrt() / mean
        });
        let times: Vec<String> = all
            .iter()
            .zip(spread)
            .map(|(library, spread)| library.map_or("-".to_string(), |library| format!("{:.2} ms ±{spread:.0}%", library.merge_ms)))
            .collect();
        merges.push(format!(
            "{name:6} {:>8}  {}   {}   {}   {}   {}{}",
            events,
            times.iter().map(|time| format!("{time:>18}")).collect::<Vec<_>>().join(" "),
            columns(all.map(|m| m.map(|m| mb(m.merging.peak)))),
            columns(all.map(|m| m.map(|m| mb(m.history)))),
            columns(all.map(|m| m.map(|m| mb(m.merging.kept)))),
            if all.iter().flatten().all(|m| m.same) { "identical" } else { "DIFFERENT" },
            if trace_text { "" } else { ", not the trace's text" }
        ));
        opens.push(format!(
            "{name:6} {}   {}   {}   {}",
            columns(all.map(|m| m.map(|m| format!("{:.2} ms", m.open_ms)))),
            columns(all.map(|m| m.map(|m| mb(m.opening.kept)))),
            columns(all.map(|m| m.map(|m| mb(m.saved)))),
            columns(all.map(|m| m.map(|m| mb(m.compressed))))
        ));
        others.push(format!(
            "{name:6} {}",
            [("yrs", &sent[0], all[2]), ("Loro", &sent[1], all[3]), ("Automerge", &sent[2], all[4])]
                .map(|(library, sent, measured)| match (sent, measured) {
                    (Ok((_, notes)), Some(measured)) => format!("{library:>10} {notes} {:>14}", if measured.same { "same text" } else { "different text" }),
                    (Err(why), _) => format!("{library:>10} {why:>28}"),
                    (Ok(_), None) => format!("{library:>10} {:>28}", "left out"),
                })
                .join("  ")
        ));
        // The replays' Yjs and Automerge documents beside the paper's own, made by those
        // libraries themselves: the time to open each and read its text, its size, and whether
        // that text is diamond-types'. A check, so one run of each will do.
        let check = |saved: Option<Vec<u8>>, open: &dyn Fn(&[u8]) -> String| {
            saved.map(|saved| {
                let t = Instant::now();
                let text = open(&saved);
                (since(t), saved.len(), text == expected)
            })
        };
        let yjs_text: &dyn Fn(&[u8]) -> String = &|bytes| yjs_open(bytes).text();
        let automerge_text: &dyn Fn(&[u8]) -> String = &|bytes| automerge_open(bytes).text();
        let replays = [
            check(sent[0].as_ref().ok().map(|(sent, _)| yjs_save(&yjs_replayed(sent))), yjs_text),
            check(sent[2].as_ref().ok().map(|(sent, _)| automerge_replayed(sent).save()), automerge_text),
        ];
        let [paper_yjs, paper_automerge] = loaded.paper.clone();
        let paper = [check(paper_yjs, yjs_text), check(paper_automerge, automerge_text)];
        papers.push(format!(
            "{name:6} {}",
            [("yrs", replays[0], paper[0]), ("Automerge", replays[1], paper[1])]
                .map(|(library, ours, paper)| {
                    let [ours, paper] = [ours, paper].map(|file| match file {
                        Some((ms, size, same)) => (format!("{ms:.2} ms"), mb(size), if same { "same text" } else { "different text" }),
                        None => ("-".to_string(), "-".to_string(), "-"),
                    });
                    format!("{library:>10} {:>11} {:>11} {:>9} {:>9} {:>14} {:>14}", ours.0, paper.0, ours.1, paper.1, ours.2, paper.2)
                })
                .join("  ")
        ));

        json.push(format!(
            "{{\"trace\":\"{name}\",\"events\":{events},\"reps\":{reps},\"trace_text\":{trace_text},\"same\":{},\"merge_ms\":{},\"open_ms\":{},\"history\":{},\"merge_peak\":{},\"merge_kept\":{},\"open_peak\":{},\"open_kept\":{},\"saved\":{},\"saved_zstd\":{},\"strider_then_ms\":[{}]}}",
            object(all.map(|m| m.map(|m| m.same))),
            object(all.map(|m| m.map(|m| format!("{:.4}", m.merge_ms)))),
            object(all.map(|m| m.map(|m| format!("{:.4}", m.open_ms)))),
            object(all.map(|m| m.map(|m| m.history))),
            object(all.map(|m| m.map(|m| m.merging.peak))),
            object(all.map(|m| m.map(|m| m.merging.kept))),
            object(all.map(|m| m.map(|m| m.opening.peak))),
            object(all.map(|m| m.map(|m| m.opening.kept))),
            object(all.map(|m| m.map(|m| m.saved))),
            object(all.map(|m| m.map(|m| m.compressed))),
            then.map(|(ms, _)| format!("{ms:.4}")).join(","),
        ));
    }

    let headings = || columns(LIBRARIES.map(|(_, name)| Some(name.to_string())));
    println!(
        "merging a history from each library's own encoding of it, until the text is read: the time, the most memory in use, the size of the encoded history, and what the document holds once done"
    );
    println!(
        "{:6} {:>8}  {}   {}   {}   {}   result",
        "trace",
        "events",
        LIBRARIES.map(|(_, name)| format!("{name:>18}")).join(" "),
        headings(),
        headings(),
        headings()
    );
    for line in merges {
        println!("{line}");
    }
    println!("\nreopening a saved document from its bytes, until the text is read: the time, what the document then holds, the size of the file, and its size compressed by zstd");
    println!("{:6} {}   {}   {}   {}", "trace", headings(), headings(), headings(), headings());
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
    println!(
        "\nthe saved documents of these replays beside the paper's own Yjs and Automerge files, which are measured above where there are any: the time to open each and read its text, its size, and whether that text is diamond-types'"
    );
    println!(
        "{:6} {}",
        "trace",
        ["yrs", "Automerge"]
            .map(|library| format!("{library:>10} {:>11} {:>11} {:>9} {:>9} {:>14} {:>14}", "replay", "paper's", "replay", "paper's", "replay", "paper's"))
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
