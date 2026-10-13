use eg_strider::{Edit, Malformed, OpLog};

/// A history with branches that merge, a run of backspaces that a later event cuts in two,
/// and text beyond ASCII.
fn history() -> OpLog {
    let mut log = OpLog::new();
    let (a, b) = (log.get_or_create_agent("a"), log.get_or_create_agent("b"));
    let base = log.add_insert(a, 0, "one two thrée four").end - 1;
    let left = log.add_delete_at(a, &[base], 2..7, false);
    log.add_insert_at(b, &[left.start + 1], 4, "🙂");
    let right = log.add_insert_at(b, &[base], 18, "!").end - 1;
    let merged = log.add_insert_at(a, &[left.end - 1, right], 0, "> ").end - 1;
    log.add_delete_at(b, &[merged], 0..1, true);
    log.add_insert(a, 0, "é");
    // A run that arrives together, with a correction of what it has just typed.
    let (c, tip) = (log.get_or_create_agent("c"), log.frontier());
    let run = [Edit::Insert { pos: 1, len: 4 }, Edit::Delete { pos: 3, len: 2, backspace: true }, Edit::Insert { pos: 3, len: 1 }];
    log.add_remote_ops(c, 0, &tip, &run, "tÿpos");
    log
}

/// Makes the same further events in a log: one from the middle of an old run, then a merge.
fn carry_on(log: &mut OpLog) {
    let (b, c) = (log.get_or_create_agent("b"), log.get_or_create_agent("c"));
    let tip = log.frontier();
    let side = log.add_insert_at(c, &[3], 2, "mid").end - 1;
    let both: Vec<usize> = tip.iter().copied().chain([side]).collect();
    log.add_insert_at(b, &both, 0, "#");
}

#[test]
fn a_saved_log_loads_with_its_document() {
    let mut log = history();
    let saved = log.save();
    let (mut loaded, text) = OpLog::load(&saved).unwrap();
    assert_eq!(text, log.checkout());
    assert_eq!(loaded.checkout(), text);
    assert_eq!(loaded.len(), log.len());
    assert_eq!(loaded.frontier(), log.frontier());
    assert_eq!(loaded.save(), saved);
}

#[test]
fn a_loaded_log_carries_on_as_the_original_does() {
    let mut log = history();
    let (mut loaded, _) = OpLog::load(&log.save()).unwrap();
    carry_on(&mut log);
    carry_on(&mut loaded);
    assert_eq!(loaded.checkout(), log.checkout());
    assert_eq!(loaded.save(), log.save());
}

#[test]
fn an_empty_log_is_saved_and_loaded() {
    let saved = OpLog::new().save();
    let (mut loaded, text) = OpLog::load(&saved).unwrap();
    assert_eq!((text.as_str(), loaded.len()), ("", 0));
    let a = loaded.get_or_create_agent("a");
    loaded.add_insert(a, 0, "hi");
    assert_eq!(loaded.checkout(), "hi");
}

#[test]
fn a_file_cut_short_is_refused() {
    let saved = history().save();
    for len in 0..saved.len() {
        assert_eq!(OpLog::load(&saved[..len]).err(), Some(Malformed), "cut to {len} bytes");
    }
    assert!(OpLog::load(&[&saved[..], &[0]].concat()).is_err(), "with a byte too many");
}

#[test]
fn a_damaged_file_is_refused_or_loaded_but_never_panics() {
    let saved = history().save();
    for at in 0..saved.len() {
        for flip in [0x01, 0x80, 0xff] {
            let mut damaged = saved.clone();
            damaged[at] ^= flip;
            let _ = OpLog::load(&damaged);
        }
    }
}
