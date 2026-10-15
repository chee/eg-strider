use crate::memory::{memory_of, Memory};
use crate::{dt_append, dt_merge, dt_open, dt_save, strider_merge};
use compare::{History, Trace};
use diamond_types::list::operation::TextOperation;
use eg_strider::OpLog;

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

/// A trace read into memory, with what each library is handed and checked against.
pub(crate) struct Loaded {
    pub(crate) name: String,
    pub(crate) path: String,
    pub(crate) events: usize,
    pub(crate) dt_history: History<TextOperation, usize>,
    /// What the history takes in diamond-types' form, and in eg-strider's.
    pub(crate) held_by_dt_history: Memory,
    pub(crate) history: History,
    pub(crate) held_by_history: Memory,
    /// diamond-types' text for the trace.
    pub(crate) expected: String,
    /// Whether that is the trace's own text.
    pub(crate) trace_text: bool,
    pub(crate) saved: Vec<u8>,
    pub(crate) dt_saved: Vec<u8>,
    /// Whether eg-strider's and diamond-types' saved documents reopen with that text.
    pub(crate) same: bool,
    /// The batches each writer of yrs, Loro and Automerge sent, with notes on the replay, or why
    /// there are none.
    pub(crate) sent: [Result<(Vec<u8>, String), &'static str>; 3],
}

/// Reads a trace and the other libraries' replays of it, replaying any not yet kept if `replay`.
pub(crate) fn load(path: &str, replay: bool, left_out: &[&str]) -> Loaded {
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
