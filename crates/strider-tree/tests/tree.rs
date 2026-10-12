use std::cmp::Ordering;
use strider_tree::{Id, Tree};
use proptest::prelude::*;
use proptest::sample::Index;

/// An operation on a tree, with its target chosen by position among the entries.
#[derive(Clone, Debug)]
enum Edit {
    Insert { before: Option<Index>, widths: [u32; 2] },
    SplitOff { at: Index, share: [Index; 2], before: bool },
    SetWidths { at: Index, widths: [u32; 2] },
    Update { at: Index, widths: [u32; 2] },
}

fn edit() -> impl Strategy<Value = Edit> {
    let widths = || [0u32..6, 0u32..6];
    prop_oneof![
        (proptest::option::weighted(0.9, any::<Index>()), widths()).prop_map(|(before, widths)| Edit::Insert { before, widths }),
        (any::<Index>(), any::<[Index; 2]>(), any::<bool>()).prop_map(|(at, share, before)| Edit::SplitOff { at, share, before }),
        (any::<Index>(), widths()).prop_map(|(at, widths)| Edit::SetWidths { at, widths }),
        (any::<Index>(), widths()).prop_map(|(at, widths)| Edit::Update { at, widths }),
    ]
}

/// Applies the edits to a tree and to a plain list, checking that every answer agrees.
fn check_against_list<const N: usize>(edits: &[Edit]) -> Result<(), TestCaseError> {
    let mut tree = Tree::<N>::new([3, 5]);
    let mut list: Vec<(Id, [u32; 2])> = vec![(0, [3, 5])];
    let offsets = |list: &[(Id, [u32; 2])]| list.iter().fold([0; 2], |t, e| [t[0] + e.1[0], t[1] + e.1[1]]);

    for edit in edits {
        match *edit {
            Edit::Insert { before, widths } => {
                let i = before.map_or(list.len(), |b| b.index(list.len()));
                let (id, at) = tree.insert(list.get(i).map(|e| tree.cursor(e.0)), widths);
                prop_assert_eq!(at, offsets(&list[..i]));
                list.insert(i, (id, widths));
            }
            Edit::SplitOff { at, share, before } => {
                let i = at.index(list.len());
                let old = list[i].1;
                let taken = [share[0].index(old[0] as usize + 1) as u32, share[1].index(old[1] as usize + 1) as u32];
                let id = tree.split_off(tree.cursor(list[i].0), taken, before);
                list[i].1 = [old[0] - taken[0], old[1] - taken[1]];
                list.insert(i + !before as usize, (id, taken));
            }
            Edit::SetWidths { at, widths } => {
                let i = at.index(list.len());
                tree.set_widths(tree.cursor(list[i].0), widths);
                list[i].1 = widths;
            }
            Edit::Update { at, widths } => {
                let i = at.index(list.len());
                prop_assert_eq!(tree.update(tree.cursor(list[i].0), widths), offsets(&list[..i]));
                list[i].1 = widths;
            }
        }
    }

    // The tree holds the same entries in the same order.
    prop_assert_eq!(tree.len(), list.len());
    let mut at = Some(tree.first());
    for &(id, widths) in &list {
        let here = at.expect("tree ended early");
        prop_assert_eq!((tree.id(here), tree.widths(here)), (id, widths));
        prop_assert_eq!(tree.cursor(id), here);
        at = tree.next(here);
    }
    prop_assert!(at.is_none());

    // Every offset of each view leads to the right entry, and back.
    for view in 0..2 {
        let mut start = 0;
        for (i, &(id, widths)) in list.iter().enumerate() {
            let here = tree.cursor(id);
            for off in 0..widths[view] {
                prop_assert_eq!(tree.seek(view, start + off), (here, off));
            }
            prop_assert_eq!(tree.offsets(here)[view], start);
            if i > 0 {
                prop_assert_eq!(tree.cmp(tree.cursor(list[i - 1].0), here), Ordering::Less);
                prop_assert_eq!(tree.cmp(tree.first(), here), Ordering::Less);
            }
            start += widths[view];
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
    let mut tree = Tree::<4>::new([1, 1]);
    for _ in 0..50 {
        tree.insert(None, [2, 0]);
    }
    tree.reset([7, 9]);
    assert_eq!(tree.len(), 1);
    assert_eq!(tree.seek(1, 8), (tree.first(), 8));
    assert!(tree.next(tree.first()).is_none());
}
