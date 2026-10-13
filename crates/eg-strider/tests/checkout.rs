use eg_strider::OpLog;

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
    let grown = log.checkout();
    log.forget();
    assert_eq!(grown, log.checkout());

    // An event concurrent with the whole history leaves no critical version but the first.
    log.add_insert_at(a, &[], 0, "~");
    let regrown = log.checkout();
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
