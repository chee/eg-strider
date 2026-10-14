use eg_strider::{Edit, EventIndex, OpLog, Rejected};

/// "hello world", then two concurrent edits and a merge.
fn log() -> OpLog {
    let mut log = OpLog::new();
    let (a, b) = (log.get_or_create_agent("a"), log.get_or_create_agent("b"));
    let start = log.add_insert(a, 0, "hello world").unwrap().end - 1;
    let left = log.add_insert_at(a, &[start], 0, "> ").unwrap().end - 1;
    let right = log.add_delete_at(b, &[start], 5..11, true).unwrap().end - 1;
    log.add_insert_at(a, &[left, right], 7, "!").unwrap();
    log
}

/// Checks that `add` is rejected for `reason` and leaves the log as it was.
fn rejected(mut log: OpLog, reason: Rejected, add: impl FnOnce(&mut OpLog) -> Result<std::ops::Range<EventIndex>, Rejected>) {
    let (len, frontier, text) = (log.len(), log.frontier(), log.checkout().to_string());
    assert_eq!(add(&mut log), Err(reason));
    assert_eq!((log.len(), log.frontier(), log.checkout()), (len, frontier, text.as_str()));
    // It carries on as if nothing had been tried.
    let a = log.get_or_create_agent("a");
    log.add_insert(a, 0, "ok ").unwrap();
    assert_eq!(log.checkout(), format!("ok {text}"));
}

#[test]
fn an_unknown_agent_or_parent_is_rejected() {
    rejected(log(), Rejected::UnknownAgent, |log| log.add_insert(7, 0, "x"));
    rejected(log(), Rejected::UnknownParent, |log| {
        let a = log.get_or_create_agent("a");
        let past = log.len() as EventIndex;
        log.add_insert_at(a, &[3, past], 0, "x")
    });
}

#[test]
fn parents_out_of_order_are_rejected() {
    for parents in [[5, 3], [3, 3]] {
        rejected(log(), Rejected::ParentsOutOfOrder, |log| {
            let a = log.get_or_create_agent("a");
            log.add_insert_at(a, &parents, 0, "x")
        });
    }
}

#[test]
fn text_that_does_not_match_the_inserts_is_rejected() {
    let run = [Edit::Insert { pos: 0, len: 2 }, Edit::Delete { pos: 0, len: 1, backspace: false }, Edit::Insert { pos: 1, len: 1 }];
    for text in ["ab", "abcd", "aé", ""] {
        rejected(log(), Rejected::TextMismatch, |log| {
            let (c, tip) = (log.get_or_create_agent("c"), log.frontier());
            log.add_remote_ops(c, 0, &tip, &run, text)
        });
    }
    // Text with no inserts at all.
    rejected(log(), Rejected::TextMismatch, |log| {
        let (c, tip) = (log.get_or_create_agent("c"), log.frontier());
        log.add_remote_ops(c, 0, &tip, &[], "x")
    });
}

#[test]
fn an_edit_past_the_end_of_the_document_is_rejected() {
    // "> hello!" is 8 characters long.
    rejected(log(), Rejected::OutOfRange, |log| {
        let a = log.get_or_create_agent("a");
        log.add_insert(a, 9, "x")
    });
    rejected(log(), Rejected::OutOfRange, |log| {
        let a = log.get_or_create_agent("a");
        log.add_delete(a, 4..9)
    });
    // In a run, each op is measured against the document as the ops before it left it.
    rejected(log(), Rejected::OutOfRange, |log| {
        let (c, tip) = (log.get_or_create_agent("c"), log.frontier());
        let run = [Edit::Delete { pos: 0, len: 8, backspace: true }, Edit::Insert { pos: 1, len: 1 }];
        log.add_remote_ops(c, 0, &tip, &run, "x")
    });
    // Up to the end is fine.
    let mut log = log();
    let a = log.get_or_create_agent("a");
    log.add_insert(a, 8, "?").unwrap();
    log.add_delete(a, 0..9).unwrap();
    assert_eq!(log.checkout(), "");
}

#[test]
fn numbers_past_32_bits_are_rejected() {
    rejected(log(), Rejected::TooLarge, |log| {
        let (c, tip) = (log.get_or_create_agent("c"), log.frontier());
        log.add_remote_insert(c, u32::MAX - 1, &tip, 0, "xyz")
    });
}

/// A long history is saved in several blocks. When the oldest are damaged, reading them back
/// for an edit made at an old version fails, and the log is as it was.
#[test]
fn damaged_saved_history_is_rejected() {
    let mut log = OpLog::new();
    let a = log.get_or_create_agent("a");
    for at in (0..100_000).step_by(4) {
        log.add_insert(a, at, "abcd").unwrap();
    }
    let mut saved = log.save();
    // The file ends with its newest block of history, which any edit at an old version needs.
    let end = saved.len();
    saved[end - 32..].iter_mut().for_each(|byte| *byte ^= 0x5a);

    rejected(OpLog::load(saved.clone()).unwrap(), Rejected::DamagedHistory, |log| {
        let b = log.get_or_create_agent("b");
        log.add_insert_at(b, &[10], 5, "old")
    });
    assert!(OpLog::load(saved).unwrap().forget().is_err());
}
