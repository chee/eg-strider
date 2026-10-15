use eg_strider::{EventIndex, Malformed, OpLog, Operation};

/// A history with branches that merge, a run of backspaces that a later event cuts in two,
/// and text beyond ASCII.
fn history() -> OpLog {
    let mut log = OpLog::new();
    let (a, b) = (log.get_or_create_replica("a"), log.get_or_create_replica("b"));
    let base = log.add_insert(a, 0, "one two thrée four").end - 1;
    let left = log.add_delete_at(a, &[base], 2..7, true).unwrap();
    log.add_insert_at(b, &[left.start + 1], 4, "🙂").unwrap();
    let right = log.add_insert_at(b, &[base], 18, "!").unwrap().end - 1;
    let merged = log.add_insert_at(a, &[left.end - 1, right], 0, "> ").unwrap().end - 1;
    log.add_delete_at(b, &[merged], 0..1, false).unwrap();
    log.add_insert(a, 0, "é");
    // A run that arrives together, with a correction of what it has just typed.
    let (c, tip) = (log.get_or_create_replica("c"), log.frontier());
    let run = [Operation::Insert { pos: 1, len: 4 }, Operation::Delete { pos: 3, len: 2, backward: true }, Operation::Insert { pos: 3, len: 1 }];
    log.add_remote_ops(c, 0, &tip, &run, "tÿpos").unwrap();
    log
}

/// Makes the same further events in a log: one from the middle of an old run, then a merge.
fn carry_on(log: &mut OpLog) {
    let (b, c) = (log.get_or_create_replica("b"), log.get_or_create_replica("c"));
    let tip = log.frontier();
    let side = log.add_insert_at(c, &[3], 2, "mid").unwrap().end - 1;
    let both: Vec<EventIndex> = tip.iter().copied().chain([side]).collect();
    log.add_insert_at(b, &both, 0, "#").unwrap();
}

/// The same history with two more events that are concurrent with each other, so that it
/// does not end at a critical version.
fn open_ended() -> OpLog {
    let mut log = history();
    let (a, b, tip) = (log.get_or_create_replica("a"), log.get_or_create_replica("b"), log.frontier());
    log.add_insert_at(a, &tip, 0, "left ").unwrap();
    log.add_insert_at(b, &tip, 3, " right").unwrap();
    log
}

#[test]
fn a_saved_log_loads_with_its_document() {
    for mut log in [history(), open_ended()] {
        let saved = log.save();
        let mut loaded = OpLog::load(saved.clone()).unwrap();
        assert_eq!(loaded.checkout(), log.checkout());
        assert_eq!((loaded.len(), loaded.is_empty()), (log.len(), false));
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
            let a = log.get_or_create_replica("a");
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

/// A long history is saved in several parts. An event made a little way back needs the last of them
/// and no more.
#[test]
fn an_event_at_an_old_version_reads_only_the_history_since() {
    const EVENTS: EventIndex = 100_000;
    let mut log = OpLog::new();
    let a = log.get_or_create_replica("a");
    for at in (0..EVENTS as usize).step_by(4) {
        log.add_insert(a, at, "abcd");
    }
    let mut loaded = OpLog::load(log.save()).unwrap();
    assert_eq!(loaded.unread_events(), EVENTS as usize);

    for (parent, text) in [(EVENTS - 10, "recent"), (EVENTS - 3, "more recent"), (10, "old")] {
        for log in [&mut log, &mut loaded] {
            let b = log.get_or_create_replica("b");
            log.add_insert_at(b, &[parent], 5, text).unwrap();
        }
        assert_eq!(loaded.checkout(), log.checkout());
        if parent > EVENTS / 2 {
            assert!((EVENTS / 2..=parent).contains(&(loaded.unread_events() as EventIndex)), "read back to event {}", loaded.unread_events());
            // What is left unread is saved as it was, and the rest again.
            let mut again = OpLog::load(loaded.save()).unwrap();
            assert_eq!(again.checkout(), log.checkout());
            again.clear_replay_cache().unwrap();
            assert_eq!(again.checkout(), log.checkout());
        } else {
            assert_eq!(loaded.unread_events(), 0);
        }
    }
    assert_eq!(OpLog::load(loaded.save()).unwrap().checkout(), log.checkout());
}

/// When saved history is damaged, reading it back for an edit made at an old version fails, and
/// the log is as it was.
#[test]
fn damaged_saved_history_is_refused_when_read_back() {
    let mut log = OpLog::new();
    let a = log.get_or_create_replica("a");
    for at in (0..100_000).step_by(4) {
        log.add_insert(a, at, "abcd");
    }
    let mut saved = log.save();
    // The file ends with its newest block of history, which any edit at an old version needs.
    let end = saved.len();
    saved[end - 32..].iter_mut().for_each(|byte| *byte ^= 0x5a);

    let mut loaded = OpLog::load(saved.clone()).unwrap();
    let (len, text) = (loaded.len(), loaded.checkout().to_string());
    let b = loaded.get_or_create_replica("b");
    assert_eq!(loaded.add_insert_at(b, &[10], 5, "old"), Err(Malformed));
    assert_eq!((loaded.len(), loaded.checkout()), (len, text.as_str()));
    // Typing on at the end needs none of it.
    loaded.add_insert(b, 0, "ok ");
    assert_eq!(loaded.checkout(), format!("ok {text}"));
    assert!(OpLog::load(saved).unwrap().clear_replay_cache().is_err());
}

#[test]
fn an_empty_log_is_saved_and_loaded() {
    let mut loaded = OpLog::load(OpLog::new().save()).unwrap();
    assert_eq!((loaded.len(), loaded.is_empty(), loaded.checkout()), (0, true, ""));
    let a = loaded.get_or_create_replica("a");
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
            let Ok(mut loaded) = OpLog::load(damaged) else { continue };
            // Reading the history back, for an edit at an old version or to start over, either
            // works or is refused.
            let b = loaded.get_or_create_replica("b");
            if loaded.add_insert_at(b, &[0], 0, "x").is_ok() && loaded.clear_replay_cache().is_ok() {
                loaded.checkout();
            }
        }
    }
}
