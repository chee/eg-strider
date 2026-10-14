use proptest::prelude::*;
use proptest::sample::Index;
use strider_text::PieceTable;

/// An edit to make, with positions picked relative to the document's length at the time.
#[derive(Clone, Debug)]
enum Step {
    Insert {
        at: Index,
        text: String,
    },
    Remove {
        at: Index,
        len: usize,
    },
    /// Carries on from where the last edit left offset, as typing and deleting do.
    Type {
        text: String,
    },
    Backspace {
        len: usize,
    },
}

fn steps(text: &'static str, max: usize) -> impl Strategy<Value = Vec<Step>> {
    let step = prop_oneof![
        2 => (any::<Index>(), text).prop_map(|(at, text)| Step::Insert { at, text }),
        2 => (any::<Index>(), 1usize..12).prop_map(|(at, len)| Step::Remove { at, len }),
        4 => text.prop_map(|text| Step::Type { text }),
        2 => (1usize..6).prop_map(|len| Step::Backspace { len }),
    ];
    proptest::collection::vec(step, 0..max)
}

/// Makes the edits to a plain list of characters, then takes them back from there.
fn check(steps: &[Step]) -> Result<(), TestCaseError> {
    let mut plain: Vec<char> = Vec::new();
    // Every step comes down to removing `removed` characters at `pos`, then inserting
    // `text` there.
    let mut made: Vec<(usize, usize, &str)> = Vec::new();
    let mut cursor = 0;
    // The document halfway through, to take the later edits back to.
    let mut halfway = String::new();
    for (i, step) in steps.iter().enumerate() {
        if i == steps.len() / 2 {
            halfway = plain.iter().collect();
        }
        let (pos, removed, text) = match step {
            Step::Insert { at, text } => (at.index(plain.len() + 1), 0, text.as_str()),
            Step::Type { text } => (cursor, 0, text.as_str()),
            Step::Remove { at, len } => {
                let pos = at.index(plain.len() + 1);
                (pos, (*len).min(plain.len() - pos), "")
            }
            Step::Backspace { len } => {
                let len = (*len).min(cursor);
                (cursor - len, len, "")
            }
        };
        plain.splice(pos..pos + removed, text.chars());
        cursor = pos + text.chars().count();
        made.push((pos, removed, text));
    }

    // The inserted texts end to end, which is where the table looks them up.
    let source: String = made.iter().map(|&(_, _, text)| text).collect();
    let mut table = PieceTable::new(plain.len() as u32, &source);
    let mut end = source.len();
    for &(pos, removed, text) in made.iter().rev() {
        table.undo_insert(pos as u32, end - text.len()..end, text.chars().count() as u32);
        table.undo_remove(pos as u32, removed as u32);
        end -= text.len();
    }
    prop_assert_eq!(table.into_string(), plain.iter().collect::<String>());

    // Taking back only the second half, and the first half all at once from its text.
    let mut table = PieceTable::new(plain.len() as u32, &source);
    let mut end = source.len();
    for &(pos, removed, text) in made[steps.len() / 2..].iter().rev() {
        table.undo_insert(pos as u32, end - text.len()..end, text.chars().count() as u32);
        table.undo_remove(pos as u32, removed as u32);
        end -= text.len();
    }
    table.undo_rest(&halfway);
    prop_assert_eq!(table.into_string(), plain.iter().collect::<String>());
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    #[test]
    fn ascii_matches_a_plain_list(steps in steps("[a-z ]{0,6}", 200)) {
        check(&steps)?;
    }

    #[test]
    fn any_text_matches_a_plain_list(steps in steps("[a-zé✓🙂]{0,6}", 200)) {
        check(&steps)?;
    }

    /// Stretches of text are copied in different ways according to their length.
    #[test]
    fn long_inserts_match_a_plain_list(steps in steps("[a-z ]{0,70}", 60)) {
        check(&steps)?;
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    #[test]
    fn long_histories_match_a_plain_list(steps in steps("[a-zé]{0,8}", 4000)) {
        check(&steps)?;
    }
}

#[test]
fn nothing_typed_gives_nothing() {
    assert_eq!(PieceTable::new(0, "").into_string(), "");
    // "gone" was typed, then deleted.
    let mut table = PieceTable::new(0, "gone");
    table.undo_remove(0, 4);
    table.undo_insert(0, 0..4, 4);
    assert_eq!(table.into_string(), "");
}

#[test]
#[should_panic(expected = "not within the document")]
fn taking_back_more_than_there_is_panics() {
    PieceTable::new(1, "x").undo_insert(1, 0..1, 1);
}

#[test]
#[should_panic(expected = "edits left")]
fn stopping_early_panics() {
    PieceTable::new(1, "x").into_string();
}
