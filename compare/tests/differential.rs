use diamond_types::list::operation::TextOperation;
use diamond_types::list::ListOpLog;
use diamond_types::LV;
use eg_strider::OpLog;
use proptest::prelude::*;
use proptest::sample::Index;

/// What one of several replicas, each with its own copy of the document, does next. Positions
/// are relative to that replica's document length at the time.
#[derive(Clone, Debug)]
enum Step {
    Insert {
        who: Index,
        at: Index,
        text: String,
    },
    Delete {
        who: Index,
        at: Index,
        len: usize,
        backspace: bool,
    },
    /// Merge everything another replica has.
    Sync {
        who: Index,
        from: Index,
    },
}

/// Histories of up to `steps` steps. `sync` is how many steps in a hundred are merges; low
/// values give long-lived branches.
fn history(steps: usize, sync: u32) -> impl Strategy<Value = Vec<Step>> {
    let step = prop_oneof![
        (100 - sync) * 3 => (any::<[Index; 2]>(), "[a-zé🙂]{1,4}").prop_map(|([who, at], text)| Step::Insert { who, at, text }),
        (100 - sync) * 2 => (any::<[Index; 2]>(), 1usize..5, any::<bool>())
            .prop_map(|([who, at], len, backspace)| Step::Delete { who, at, len, backspace }),
        sync * 5 => any::<[Index; 2]>().prop_map(|[who, from]| Step::Sync { who, from }),
    ];
    proptest::collection::vec(step, 1..=steps)
}

/// With `apart`, the replicas start from a shared text and each edits around its own part of
/// it, so that much of what they do at once does not collide.
fn build(replicas: usize, steps: &[Step], apart: bool) -> ListOpLog {
    let mut dt = ListOpLog::new();
    // Some replicas share an agent name, so that ties between an agent and itself occur.
    let agents: Vec<_> = (0..replicas).map(|i| dt.get_or_create_agent_id(&format!("agent{}", (i * 7) % 5))).collect();
    let mut versions: Vec<Vec<LV>> = vec![Vec::new(); replicas];
    if apart {
        let shared: String = (0..30 * replicas).map(|i| (b'A' + (i % 26) as u8) as char).collect();
        versions = vec![vec![dt.add_insert_at(agents[0], &[], 0, &shared)]; replicas];
    }
    // A position in a document of `len` characters, or just past its end.
    let place = |who: usize, at: &Index, len: usize| {
        if apart {
            (len * (2 * who + 1) / (2 * replicas) + at.index(7)).saturating_sub(3).min(len)
        } else {
            at.index(len + 1)
        }
    };

    for step in steps {
        match step {
            Step::Insert { who, at, text } => {
                let who = who.index(replicas);
                let len = dt.checkout(&versions[who]).len();
                versions[who] = vec![dt.add_insert_at(agents[who], &versions[who], place(who, at, len), text)];
            }
            Step::Delete { who, at, len, backspace } => {
                let who = who.index(replicas);
                let doc_len = dt.checkout(&versions[who]).len();
                if doc_len == 0 {
                    continue;
                }
                let start = place(who, at, doc_len - 1);
                let mut op = TextOperation::new_delete(start..(start + len).min(doc_len));
                op.loc.fwd = !backspace;
                versions[who] = vec![dt.add_operations_at(agents[who], &versions[who], &[op])];
            }
            Step::Sync { who, from } => {
                let (who, from) = (who.index(replicas), from.index(replicas));
                versions[who] = dt.version_union(&versions[who], &versions[from]).as_ref().to_vec();
            }
        }
    }
    dt
}

/// Checks that a history gives the same document in eg-strider as in diamond-types: on a first
/// checkout, on a repeat, after saving and loading, and when the log is checked out again and
/// again as it grows.
fn check(replicas: usize, steps: &[Step], apart: bool) -> Result<(), TestCaseError> {
    let dt = build(replicas, steps, apart);
    let expected = dt.checkout_tip().content().to_string();

    let mut log = compare::convert(&dt);
    prop_assert_eq!(log.checkout(), expected.as_str(), "first checkout");
    prop_assert_eq!(log.checkout(), expected.as_str(), "second checkout");
    let mut loaded = OpLog::load(log.save()).expect("a saved log loads");
    prop_assert_eq!(loaded.checkout(), expected.as_str(), "the saved document");
    loaded.forget();
    prop_assert_eq!(loaded.checkout(), expected.as_str(), "merged again from the saved history");

    // Growing a little at a time and checking out at each step, so that regions are extended
    // and absorbed while the log holds what it resolved for their earlier shapes. A second
    // log grows the same way but is saved and loaded at each step, so that it carries on from
    // a document whose history it has not read.
    let history = compare::History::of(&dt).edits();
    let (mut growing, mut reloaded) = (OpLog::new(), OpLog::new());
    for part in 1..=8 {
        let limit = if part == 8 { usize::MAX } else { dt.len() * part / 8 };
        compare::extend(&mut growing, &history, limit);
        let mut fresh = OpLog::new();
        compare::extend(&mut fresh, &history, limit);
        prop_assert_eq!(growing.checkout(), fresh.checkout(), "after growing to part {} of 8", part);

        compare::extend(&mut reloaded, &history, limit);
        prop_assert_eq!(reloaded.checkout(), fresh.checkout(), "after loading and growing to part {} of 8", part);
        reloaded = OpLog::load(reloaded.save()).expect("a saved log loads");
        prop_assert_eq!(reloaded.checkout(), fresh.checkout(), "loaded at part {} of 8", part);
    }
    prop_assert_eq!(growing.checkout(), expected.as_str(), "after growing");
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn two_replicas_syncing_often(steps in history(60, 30)) {
        check(2, &steps, false)?;
    }

    #[test]
    fn two_replicas_working_apart(steps in history(80, 25)) {
        check(2, &steps, true)?;
    }

    #[test]
    fn four_replicas_working_apart(steps in history(200, 15)) {
        check(4, &steps, true)?;
    }

    #[test]
    fn three_replicas_with_long_branches(steps in history(120, 8)) {
        check(3, &steps, false)?;
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(120))]

    #[test]
    fn many_replicas(replicas in 4usize..8, steps in history(200, 15)) {
        check(replicas, &steps, false)?;
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(12))]

    #[test]
    fn long_histories_working_apart(steps in history(1500, 20)) {
        check(4, &steps, true)?;
    }

    #[test]
    fn long_histories(steps in history(1500, 10)) {
        check(4, &steps, false)?;
    }
}
