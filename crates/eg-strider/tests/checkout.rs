use eg_strider::{Edit, OpLog};

/// Two replicas rewrite different ends of a shared text at once. Nothing either does lands
/// among what the other did, so the tracker is never needed.
#[test]
fn concurrent_edits_that_keep_apart_are_merged_by_shifting() {
    let mut log = OpLog::new();
    let (a, b) = (log.get_or_create_agent("a"), log.get_or_create_agent("b"));
    let base = [log.add_insert(a, 0, "one two three four").end - 1];
    let x = log.add_delete_at(a, &base, 0..3, true); // " two three four"
    let x = log.add_insert_at(a, &[x.end - 1], 0, "1"); // "1 two three four"
    let y = log.add_insert_at(b, &base, 18, "!"); // "one two three four!"
    let y = log.add_delete_at(b, &[y.end - 1], 14..18, false); // "one two three !"
    let y = log.add_insert_at(b, &[y.end - 1], 14, "4"); // "one two three 4!"
    log.add_insert_at(a, &[x.end - 1, y.end - 1], 1, "st");
    assert_eq!(log.checkout(), "1st two three 4!");
    assert_eq!(log.tracked_regions(), 0);
    assert_eq!(log.checkout(), "1st two three 4!");
}

#[test]
fn concurrent_edits_at_one_place_go_to_the_tracker() {
    let mut log = OpLog::new();
    let (a, b) = (log.get_or_create_agent("a"), log.get_or_create_agent("b"));
    let base = [log.add_insert(a, 0, "<>").end - 1];
    log.add_insert_at(a, &base, 1, "x");
    log.add_insert_at(b, &base, 1, "y");
    assert_eq!(log.checkout(), "<xy>");
    assert_eq!(log.tracked_regions(), 1);
    assert_eq!(log.checkout(), "<xy>");
}

#[test]
fn sequential_edits() {
    let mut log = OpLog::new();
    let a = log.get_or_create_agent("a");
    log.add_insert(a, 0, "hello");
    log.add_insert(a, 5, " world");
    log.add_delete(a, 0..1);
    log.add_insert(a, 0, "H");
    assert_eq!(log.checkout(), "Hello world");
}

/// Figure 4 of the paper: "hi" becomes "hey" on one branch and "Hi" on the other, then "!"
/// is appended after merging.
#[test]
fn paper_figure_4() {
    let mut log = OpLog::new();
    let (a, b) = (log.get_or_create_agent("a"), log.get_or_create_agent("b"));
    let base = log.add_insert(a, 0, "hi").end - 1;
    log.add_insert_at(a, &[base], 0, "H");
    let left = log.add_delete_at(a, &[base + 1], 1..2, true).end - 1;
    log.add_delete_at(b, &[base], 1..2, true);
    let right = log.add_insert(b, 1, "ey").end - 1;
    log.add_insert_at(a, &[left, right], 3, "!");
    assert_eq!(log.checkout(), "Hey!");
}

#[test]
fn concurrent_inserts_at_one_position_do_not_interleave() {
    let mut log = OpLog::new();
    let (a, b) = (log.get_or_create_agent("a"), log.get_or_create_agent("b"));
    log.add_insert_at(a, &[], 0, "aaa");
    log.add_insert_at(b, &[], 0, "bbb");
    assert_eq!(log.checkout(), "aaabbb");
}

#[test]
fn concurrent_deletes_of_the_same_text() {
    let mut log = OpLog::new();
    let (a, b) = (log.get_or_create_agent("a"), log.get_or_create_agent("b"));
    let base = log.add_insert(a, 0, "abcdef").end - 1;
    let left = log.add_delete_at(a, &[base], 1..4, true).end - 1;
    let right = log.add_delete_at(b, &[base], 2..5, false).end - 1;
    log.add_insert_at(a, &[left, right], 1, "X");
    assert_eq!(log.checkout(), "aXf");
}

#[test]
fn checking_out_again_and_after_growing_gives_the_same_document() {
    let mut log = OpLog::new();
    let (a, b) = (log.get_or_create_agent("a"), log.get_or_create_agent("b"));
    let base = log.add_insert(a, 0, "0123456789").end - 1;
    let left = log.add_insert_at(a, &[base], 3, "aa").end - 1;
    let right = log.add_delete_at(b, &[base], 2..6, false).end - 1;
    let merged = log.add_insert_at(b, &[left, right], 2, "!").end - 1;
    assert_eq!(log.checkout(), "01!aa6789");
    assert_eq!(log.checkout(), "01!aa6789");

    // A late branch from before the merge reopens the region and extends it.
    log.add_insert_at(a, &[left], 0, ">");
    log.add_insert_at(b, &[merged], 0, "#");
    let grown = log.checkout().to_string();
    log.forget();
    assert_eq!(grown, log.checkout());

    // An event concurrent with the whole history leaves no critical version but the first.
    log.add_insert_at(a, &[], 0, "~");
    let regrown = log.checkout().to_string();
    log.forget();
    assert_eq!(regrown, log.checkout());
}

/// A run of backspaces is one op, but each of its events removes one particular character.
/// An event made after only the first two of them cuts the run in two, and to merge it the
/// characters that the second half removed have to be put back.
#[test]
fn an_event_made_partway_through_a_run_of_backspaces() {
    let mut log = OpLog::new();
    let (a, b, c) = (log.get_or_create_agent("a"), log.get_or_create_agent("b"), log.get_or_create_agent("c"));
    let base = log.add_insert(a, 0, "abcdef").end - 1;
    let backspaces = log.add_delete_at(a, &[base], 2..6, false); // "ab", removing f, e, d, c in turn
    log.add_insert_at(c, &[base], 4, "Y"); // "abcdYef"
    assert_eq!(log.checkout(), "abY");
    assert_eq!(log.tracked_regions(), 1);
    // After the first two backspaces the text was "abcd".
    log.add_insert_at(b, &[backspaces.start + 1], 3, "X"); // "abcXd"
    assert_eq!(log.checkout(), "abXY");
}

/// A run typed, corrected and typed again: "héllo" loses its end to two backspaces, and what is
/// typed next is deleted whole. Returns the ops and the text they insert.
fn corrected_typing() -> (Vec<Edit>, &'static str) {
    let ops = vec![
        Edit::Insert { pos: 0, len: 5 },
        Edit::Delete { pos: 3, len: 2, backspace: true },
        Edit::Delete { pos: 2, len: 1, backspace: true },
        Edit::Insert { pos: 2, len: 3 },
        Edit::Delete { pos: 2, len: 3, backspace: false },
        Edit::Insert { pos: 0, len: 1 },
        Edit::Delete { pos: 1, len: 1, backspace: false },
    ];
    (ops, "hélloy!🙂A")
}

/// Appends the same ops one call at a time, each following the one before.
fn one_at_a_time(log: &mut OpLog, agent: u32, ops: &[Edit], text: &str) {
    let (mut seq, mut rest) = (0, text);
    let mut parents = log.frontier();
    for op in ops {
        let events = match *op {
            Edit::Insert { pos, len } => {
                let bytes = rest.char_indices().nth(len as usize).map_or(rest.len(), |(at, _)| at);
                let (inserted, after) = rest.split_at(bytes);
                rest = after;
                log.add_remote_insert(agent, seq, &parents, pos as usize, inserted)
            }
            Edit::Delete { pos, len, backspace } => log.add_remote_delete(agent, seq, &parents, pos as usize..(pos + len) as usize, !backspace),
        };
        seq += events.len();
        parents = vec![events.end - 1];
    }
}

#[test]
fn ops_arriving_as_a_run_merge_as_they_do_one_at_a_time() {
    let (ops, text) = corrected_typing();
    let (mut together, mut apart) = (OpLog::new(), OpLog::new());
    let (a, b) = (together.get_or_create_agent("a"), apart.get_or_create_agent("a"));
    let events = together.add_remote_ops(a, 0, &[], &ops, text);
    one_at_a_time(&mut apart, b, &ops, text);
    assert_eq!(events, 0..apart.len());
    assert_eq!(together.checkout(), "A\u{e9}");
    assert_eq!(apart.checkout(), "A\u{e9}");

    // An event made after only part of the first insert puts that insert in a concurrent
    // part of the history, where its corrections are merged like any other deletes.
    for log in [&mut together, &mut apart] {
        let c = log.get_or_create_agent("c");
        log.add_insert_at(c, &[2], 1, "<>");
    }
    assert_eq!(together.checkout(), apart.checkout());
}

#[test]
fn a_run_that_follows_earlier_typing_is_merged_with_it() {
    let (ops, text) = corrected_typing();
    let (mut together, mut apart) = (OpLog::new(), OpLog::new());
    for log in [&mut together, &mut apart] {
        let a = log.get_or_create_agent("a");
        log.add_insert(a, 0, "xyz");
    }
    let (a, tip) = (together.get_or_create_agent("b"), together.frontier());
    together.add_remote_ops(a, 0, &tip, &ops, text);
    let b = apart.get_or_create_agent("b");
    one_at_a_time(&mut apart, b, &ops, text);
    assert_eq!(together.checkout(), apart.checkout());
    assert_eq!(together.checkout(), "A\u{e9}xyz");
}

/// A run of typing, or of deleting, is one op. A merge halfway through it leaves a document
/// that holds half of the op, and an event made before the run began is concurrent with
/// both halves.
#[test]
fn an_event_from_before_the_last_merge_is_merged_into_it() {
    for (deleting, expected) in [(false, "01x234é🙂é🙂y56789!"), (true, "01xy6789!")] {
        let build = |merge_halfway: bool| {
            let mut log = OpLog::new();
            let (a, b) = (log.get_or_create_agent("a"), log.get_or_create_agent("b"));
            let start = log.add_insert(a, 0, "0123456789").end - 1;
            let half = |log: &mut OpLog, at: usize| if deleting { log.add_delete(a, 2..4) } else { log.add_insert(a, at, "é🙂") };
            half(&mut log, 5);
            if merge_halfway {
                log.checkout();
            }
            let tip = half(&mut log, 7).end - 1;
            // One away from the run, and one at the very place it began.
            let side = log.add_insert_at(b, &[start], 2, "x").end - 1;
            let side = log.add_insert_at(b, &[side], 6, "y").end - 1;
            let apart = log.checkout().to_string();
            let end = log.checkout().chars().count();
            log.add_insert_at(a, &[tip, side], end, "!");
            (apart, log.checkout().to_string())
        };
        assert_eq!(build(true), build(false));
        assert_eq!(build(true).1, expected);
    }
}
