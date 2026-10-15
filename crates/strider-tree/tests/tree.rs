use std::cmp::Ordering;
use strider_tree::{EntryId, Lengths, PrepareEffectTree, View};

/// Lengths as the tree takes them, from a (prepare, effect) pair.
fn w(pair: [u32; 2]) -> Lengths {
    Lengths { prepare: pair[0], effect: pair[1] }
}
use proptest::prelude::*;
use proptest::sample::Index;

/// An operation on a tree, with its target chosen by position among the entries.
#[derive(Clone, Debug)]
enum Edit {
    Insert {
        before: Option<Index>,
        lengths: [u32; 2],
    },
    SplitEntry {
        at: Index,
        share: [Index; 2],
        before: bool,
    },
    SetWidths {
        at: Index,
        lengths: [u32; 2],
    },
    Update {
        at: Index,
        lengths: [u32; 2],
    },
    /// Looks up a character index, which the tree remembers the whereabouts of.
    Find {
        prepare: bool,
        place: Index,
    },
}

fn edit() -> impl Strategy<Value = Edit> {
    let lengths = || [0u32..6, 0u32..6];
    prop_oneof![
        (proptest::option::weighted(0.9, any::<Index>()), lengths()).prop_map(|(before, lengths)| Edit::Insert { before, lengths }),
        (any::<Index>(), any::<[Index; 2]>(), any::<bool>()).prop_map(|(at, share, before)| Edit::SplitEntry { at, share, before }),
        (any::<Index>(), lengths()).prop_map(|(at, lengths)| Edit::SetWidths { at, lengths }),
        (any::<Index>(), lengths()).prop_map(|(at, lengths)| Edit::Update { at, lengths }),
        (any::<bool>(), any::<Index>()).prop_map(|(prepare, place)| Edit::Find { prepare, place }),
    ]
}

/// Applies the edits to a tree and to a plain list, checking that every answer agrees.
fn check_against_list<const N: usize>(edits: &[Edit]) -> Result<(), TestCaseError> {
    let mut tree = PrepareEffectTree::<N>::new(w([3, 5]));
    let mut list: Vec<(EntryId, [u32; 2])> = vec![(0, [3, 5])];
    let effect_index = |list: &[(EntryId, [u32; 2])]| list.iter().map(|e| e.1[1]).sum::<u32>();

    for edit in edits {
        match *edit {
            Edit::Insert { before, lengths } => {
                let i = before.map_or(list.len(), |b| b.index(list.len()));
                // Every other insert is by a caller that does not ask where it landed.
                let before = list.get(i).map(|e| tree.cursor(e.0));
                let id = if list.len().is_multiple_of(2) {
                    tree.add(before, w(lengths))
                } else {
                    let (id, at) = tree.insert(before, w(lengths));
                    prop_assert_eq!(at, effect_index(&list[..i]));
                    id
                };
                list.insert(i, (id, lengths));
            }
            Edit::SplitEntry { at, share, before } => {
                let i = at.index(list.len());
                let old = list[i].1;
                let taken = [share[0].index(old[0] as usize + 1) as u32, share[1].index(old[1] as usize + 1) as u32];
                let (id, parts) = tree.split_entry(tree.cursor(list[i].0), w(taken), before);
                list[i].1 = [old[0] - taken[0], old[1] - taken[1]];
                list.insert(i + !before as usize, (id, taken));
                prop_assert_eq!(parts.map(|part| tree.id(part)), [list[i].0, list[i + 1].0]);
            }
            Edit::SetWidths { at, lengths } => {
                let i = at.index(list.len());
                tree.set_lengths(tree.cursor(list[i].0), w(lengths));
                list[i].1 = lengths;
            }
            Edit::Update { at, lengths } => {
                let i = at.index(list.len());
                prop_assert_eq!(tree.update(tree.cursor(list[i].0), w(lengths)), effect_index(&list[..i]));
                list[i].1 = lengths;
            }
            Edit::Find { prepare, place } => {
                let (view, view_index) = if prepare { (View::Prepare, 0) } else { (View::Effect, 1) };
                let total: u32 = list.iter().map(|e| e.1[view_index]).sum();
                if total > 0 {
                    let mut index = place.index(total as usize) as u32;
                    let (at, offset, effect) = tree.locate(view, index);
                    // The first entry that `index` falls within, and the effect lengths before it.
                    let mut before = 0;
                    let entry = list.iter().find(|e| {
                        let within = index < e.1[view_index];
                        index -= if within { 0 } else { e.1[view_index] };
                        before += if within { 0 } else { e.1[1] };
                        within
                    });
                    prop_assert_eq!((tree.id(at), offset, effect), (entry.expect("within the total").0, index, before));
                }
            }
        }
    }

    // The tree holds the same entries in the same order.
    prop_assert_eq!(tree.len(), list.len());
    let mut at = Some(tree.first());
    for &(id, lengths) in &list {
        let here = at.expect("tree ended early");
        prop_assert_eq!((tree.id(here), tree.lengths(here)), (id, w(lengths)));
        prop_assert_eq!(tree.cursor(id), here);
        at = tree.next(here);
    }
    prop_assert!(at.is_none());

    // In each view, every character index leads to the right entry, and back.
    for (view, view_index) in [(View::Prepare, 0), (View::Effect, 1)] {
        let mut start = 0;
        for (i, &(id, lengths)) in list.iter().enumerate() {
            let here = tree.cursor(id);
            for offset in 0..lengths[view_index] {
                prop_assert_eq!(tree.char_index_to_entry(view, start + offset), (here, offset));
            }
            prop_assert_eq!(tree.entry_to_char_index(view, here), start);
            if i > 0 {
                prop_assert_eq!(tree.cmp(tree.cursor(list[i - 1].0), here), Ordering::Less);
                prop_assert_eq!(tree.cmp(tree.first(), here), Ordering::Less);
            }
            start += lengths[view_index];
        }
    }
    Ok(())
}

proptest! {
    /// With four entries to a node, a few hundred edits build a tree several levels deep.
    #[test]
    fn matches_a_plain_list_with_tiny_nodes(edits in proptest::collection::vec(edit(), 0..400)) {
        check_against_list::<4>(&edits)?;
    }

    #[test]
    fn matches_a_plain_list_with_wide_nodes(edits in proptest::collection::vec(edit(), 0..1500)) {
        check_against_list::<32>(&edits)?;
    }
}

#[test]
fn reset_starts_over() {
    let mut tree = PrepareEffectTree::<4>::new(w([1, 1]));
    for _ in 0..50 {
        tree.insert(None, w([2, 0]));
    }
    tree.reset(w([7, 9]));
    assert_eq!(tree.len(), 1);
    assert_eq!(tree.char_index_to_entry(View::Effect, 8), (tree.first(), 8));
    assert_eq!(tree.entry_to_char_index(View::Effect, tree.first()), 0);
    assert!(tree.next(tree.first()).is_none());
}
