use eg_strider::{Edit, EventId, Malformed, OpLog};

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
    let both: Vec<EventId> = tip.iter().copied().chain([side]).collect();
    log.add_insert_at(b, &both, 0, "#");
}

/// The same history with two more events that are concurrent with each other, so that it
/// does not end at a critical version.
fn open_ended() -> OpLog {
    let mut log = history();
    let (a, b, tip) = (log.get_or_create_agent("a"), log.get_or_create_agent("b"), log.frontier());
    log.add_insert_at(a, &tip, 0, "left ");
    log.add_insert_at(b, &tip, 3, " right");
    log
}

#[test]
fn a_saved_log_loads_with_its_document() {
    for mut log in [history(), open_ended()] {
        let saved = log.save();
        let mut loaded = OpLog::load(saved.clone()).unwrap();
        assert_eq!(loaded.checkout(), log.checkout());
        assert_eq!(loaded.len(), log.len());
        assert_eq!(loaded.frontier(), log.frontier());
        assert_eq!(loaded.save(), saved);
    }
}

#[test]
fn a_loaded_log_carries_on_as_the_original_does() {
    for mut log in [history(), open_ended()] {
        let mut loaded = OpLog::load(log.save()).unwrap();
        // Typing on at the end needs none of the saved history.
        for log in [&mut log, &mut loaded] {
            let a = log.get_or_create_agent("a");
            log.add_insert(a, 1, "on ");
        }
        assert_eq!(loaded.checkout(), log.checkout());
        // An event made at an old version does.
        carry_on(&mut log);
        carry_on(&mut loaded);
        assert_eq!(loaded.checkout(), log.checkout());
        let mut again = OpLog::load(loaded.save()).unwrap();
        assert_eq!(again.checkout(), log.checkout());
    }
}

/// A long history is saved in several blocks. An event made a little way back needs the
/// last of them and no more.
#[test]
fn an_event_at_an_old_version_reads_only_the_history_since() {
    const EVENTS: EventId = 100_000;
    let mut log = OpLog::new();
    let a = log.get_or_create_agent("a");
    for at in (0..EVENTS as usize).step_by(4) {
        log.add_insert(a, at, "abcd");
    }
    let mut loaded = OpLog::load(log.save()).unwrap();
    assert_eq!(loaded.unread(), EVENTS as usize);

    for (parent, text) in [(EVENTS - 10, "recent"), (EVENTS - 3, "more recent"), (10, "old")] {
        for log in [&mut log, &mut loaded] {
            let b = log.get_or_create_agent("b");
            log.add_insert_at(b, &[parent], 5, text);
        }
        assert_eq!(loaded.checkout(), log.checkout());
        if parent > EVENTS / 2 {
            assert!((EVENTS / 2..=parent).contains(&(loaded.unread() as EventId)), "read back to event {}", loaded.unread());
            // What is left unread is saved as it was, and the rest again.
            let mut again = OpLog::load(loaded.save()).unwrap();
            assert_eq!(again.checkout(), log.checkout());
            again.forget();
            assert_eq!(again.checkout(), log.checkout());
        } else {
            assert_eq!(loaded.unread(), 0);
        }
    }
    assert_eq!(OpLog::load(loaded.save()).unwrap().checkout(), log.checkout());
}

#[test]
fn an_empty_log_is_saved_and_loaded() {
    let mut loaded = OpLog::load(OpLog::new().save()).unwrap();
    assert_eq!((loaded.len(), loaded.checkout()), (0, ""));
    let a = loaded.get_or_create_agent("a");
    loaded.add_insert(a, 0, "hi");
    assert_eq!(loaded.checkout(), "hi");
}

#[test]
fn a_file_cut_short_is_refused() {
    for mut log in [history(), open_ended()] {
        let saved = log.save();
        for len in 0..saved.len() {
            assert_eq!(OpLog::load(saved[..len].to_vec()).err(), Some(Malformed), "cut to {len} bytes");
        }
        assert!(OpLog::load([&saved[..], &[0]].concat()).is_err(), "with a byte too many");
    }
}

#[test]
fn a_damaged_file_is_refused_or_loaded_but_never_panics() {
    let saved = open_ended().save();
    for at in 0..saved.len() {
        for flip in [0x01, 0x80, 0xff] {
            let mut damaged = saved.clone();
            damaged[at] ^= flip;
            let _ = OpLog::load(damaged);
        }
    }
}
