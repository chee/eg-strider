use std::cmp::Ordering;
use strider_tree::{Id, PrepareEffectTree, View, Widths};

/// Widths as the tree takes them, from a (prepare, effect) pair.
fn w(pair: [u32; 2]) -> Widths {
    Widths { prepare: pair[0], effect: pair[1] }
}
use proptest::prelude::*;
use proptest::sample::Index;

/// An operation on a tree, with its target chosen by position among the entries.
#[derive(Clone, Debug)]
enum Edit {
    Insert { before: Option<Index>, widths: [u32; 2] },
    SplitEntry { at: Index, share: [Index; 2], before: bool },
    SetWidths { at: Index, widths: [u32; 2] },
    Update { at: Index, widths: [u32; 2] },
    /// Looks up a character index, which the tree remembers the whereabouts of.
    Find { prepare: bool, idx: Index },
}

fn edit() -> impl Strategy<Value = Edit> {
    let widths = || [0u32..6, 0u32..6];
    prop_oneof![
        (proptest::option::weighted(0.9, any::<Index>()), widths()).prop_map(|(before, widths)| Edit::Insert { before, widths }),
        (any::<Index>(), any::<[Index; 2]>(), any::<bool>()).prop_map(|(at, share, before)| Edit::SplitEntry { at, share, before }),
        (any::<Index>(), widths()).prop_map(|(at, widths)| Edit::SetWidths { at, widths }),
        (any::<Index>(), widths()).prop_map(|(at, widths)| Edit::Update { at, widths }),
        (any::<bool>(), any::<Index>()).prop_map(|(prepare, idx)| Edit::Find { prepare, idx }),
    ]
}

/// Applies the edits to a tree and to a plain list, checking that every answer agrees.
fn check_against_list<const N: usize>(edits: &[Edit]) -> Result<(), TestCaseError> {
    let mut tree = PrepareEffectTree::<N>::new(w([3, 5]));
    let mut list: Vec<(Id, [u32; 2])> = vec![(0, [3, 5])];
    let effect_idx = |list: &[(Id, [u32; 2])]| list.iter().map(|e| e.1[1]).sum::<u32>();

    for edit in edits {
        match *edit {
            Edit::Insert { before, widths } => {
                let i = before.map_or(list.len(), |b| b.index(list.len()));
                // Every other insert is by a caller that does not ask where it landed.
                let before = list.get(i).map(|e| tree.cursor(e.0));
                let id = if list.len().is_multiple_of(2) {
                    tree.add(before, w(widths))
                } else {
                    let (id, at) = tree.insert(before, w(widths));
                    prop_assert_eq!(at, effect_idx(&list[..i]));
                    id
                };
                list.insert(i, (id, widths));
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
            Edit::SetWidths { at, widths } => {
                let i = at.index(list.len());
                tree.set_widths(tree.cursor(list[i].0), w(widths));
                list[i].1 = widths;
            }
            Edit::Update { at, widths } => {
                let i = at.index(list.len());
                prop_assert_eq!(tree.update(tree.cursor(list[i].0), w(widths)), effect_idx(&list[..i]));
                list[i].1 = widths;
            }
            Edit::Find { prepare, idx } => {
                let (view, k) = if prepare { (View::Prepare, 0) } else { (View::Effect, 1) };
                let total: u32 = list.iter().map(|e| e.1[k]).sum();
                if total > 0 {
                    let mut idx = idx.index(total as usize) as u32;
                    let (at, off, effect) = tree.locate(view, idx);
                    // The first entry that `idx` falls within, and the effect widths before it.
                    let mut before = 0;
                    let entry = list.iter().find(|e| {
                        let within = idx < e.1[k];
                        idx -= if within { 0 } else { e.1[k] };
                        before += if within { 0 } else { e.1[1] };
                        within
                    });
                    prop_assert_eq!((tree.id(at), off, effect), (entry.expect("within the total").0, idx, before));
                }
            }
        }
    }

    // The tree holds the same entries in the same order.
    prop_assert_eq!(tree.len(), list.len());
    let mut at = Some(tree.first());
    for &(id, widths) in &list {
        let here = at.expect("tree ended early");
        prop_assert_eq!((tree.id(here), tree.widths(here)), (id, w(widths)));
        prop_assert_eq!(tree.cursor(id), here);
        at = tree.next(here);
    }
    prop_assert!(at.is_none());

    // In each view, every character index leads to the right entry, and back.
    for (view, k) in [(View::Prepare, 0), (View::Effect, 1)] {
        let mut start = 0;
        for (i, &(id, widths)) in list.iter().enumerate() {
            let here = tree.cursor(id);
            for off in 0..widths[k] {
                prop_assert_eq!(tree.char_idx_to_entry(view, start + off), (here, off));
            }
            prop_assert_eq!(tree.entry_to_char_idx(view, here), start);
            if i > 0 {
                prop_assert_eq!(tree.cmp(tree.cursor(list[i - 1].0), here), Ordering::Less);
                prop_assert_eq!(tree.cmp(tree.first(), here), Ordering::Less);
            }
            start += widths[k];
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
    assert_eq!(tree.char_idx_to_entry(View::Effect, 8), (tree.first(), 8));
    assert_eq!(tree.entry_to_char_idx(View::Effect, tree.first()), 0);
    assert!(tree.next(tree.first()).is_none());
}
