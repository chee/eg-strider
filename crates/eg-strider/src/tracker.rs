//! The tracker: the temporary CRDT state for merging one region, as in Eg-walker. It holds
//! the region's inserted text as spans (the items of a sequence CRDT such as Yjs or Fugue,
//! with left and right origins) in a B+ tree that measures them in two versions: the one an
//! event was made in (prepare) and the merged document (effect). Merging an event means
//! finding its position in the first and reading it off in the second. Flat arrays map each
//! event to its span or its delete target, for moving the prepare version.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::ops::Range;
use strider_tree::{Cursor, PrepareEffectTree, View, Widths};

/// No item: the start of the document as a left origin, the end as a right origin.
pub(crate) const NONE: u32 = u32::MAX;

/// Items with this bit set are characters of the document as it stood when the region began
/// ("underwater"): `UNDERWATER | position`.
const UNDERWATER: u32 = 1 << 31;
/// The placeholder for the pre-region document. Its real length is not needed.
const UNDERWATER_LEN: u32 = 0x7fff_0000;

// A span's state in the prepare version. Each delete of an inserted character adds one.
const NOT_INSERTED_YET: u32 = 0;
const INSERTED: u32 = 1;

/// Tree nodes are tiny in debug builds so that tests on small documents still split them.
const NODE: usize = if cfg!(debug_assertions) { 4 } else { 32 };

#[derive(Clone, Copy)]
struct Span {
    /// The first character; the rest follow consecutively.
    item: u32,
    len: u32,
    /// The item to the left of the first character when it was inserted. Each later
    /// character's left origin is the character before it.
    origin_left: u32,
    /// The first item to the right that existed when these were inserted.
    origin_right: u32,
    state: u32,
    ever_deleted: bool,
}

impl Span {
    /// Returns the length as seen in the prepare version and in the effect version.
    fn widths(&self) -> Widths {
        Widths {
            prepare: if self.state == INSERTED { self.len } else { 0 },
            effect: if self.ever_deleted { 0 } else { self.len },
        }
    }
}

/// A place between two characters.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Place {
    /// This many characters into the span at this tree entry; fewer than its length.
    In(Cursor, u32),
    End,
}

pub(crate) struct Tracker {
    /// The spans in document order, measured in the prepare and effect versions.
    tree: PrepareEffectTree<NODE>,
    /// By tree entry id.
    spans: Vec<Span>,
    /// Per region event: the span holding the character it inserted, or `NONE` for a delete.
    ev_span: Vec<u32>,
    /// Per region event: the item it deleted. Unused for inserts.
    ev_target: Vec<u32>,
    /// Underwater spans by starting position.
    underwater: BTreeMap<u32, u32>,
    /// Per span: the `scan_stamp` of the last `integrate` scan that passed over it.
    scanned: Vec<u32>,
    scan_stamp: u32,
}

impl Default for Tracker {
    fn default() -> Self {
        Tracker {
            tree: PrepareEffectTree::new(Widths::default()),
            spans: Vec::new(),
            ev_span: Vec::new(),
            ev_target: Vec::new(),
            underwater: BTreeMap::new(),
            scanned: Vec::new(),
            scan_stamp: 0,
        }
    }
}

impl Tracker {
    /// Resets to a single placeholder for the document as it stands, ready for a region of
    /// `events` events.
    pub fn begin_region(&mut self, events: usize) {
        let placeholder = Span {
            item: UNDERWATER,
            len: UNDERWATER_LEN,
            origin_left: NONE,
            origin_right: NONE,
            state: INSERTED,
            ever_deleted: false,
        };
        self.tree.reset(placeholder.widths());
        self.spans.clear();
        self.spans.push(placeholder);
        self.underwater.clear();
        self.underwater.insert(0, 0);
        self.scanned.clear();
        self.scan_stamp = 0;
        self.ev_span.clear();
        self.ev_span.resize(events, NONE);
        self.ev_target.clear();
        self.ev_target.resize(events, NONE);
    }

    // ---------------------------------------------------------------- finding things

    /// Returns the span stored at a tree entry.
    fn span(&self, at: Cursor) -> &Span {
        &self.spans[self.tree.id(at) as usize]
    }

    /// Finds the span holding `item`, and how far into it.
    fn locate(&self, item: u32) -> (u32, u32) {
        let span = if item & UNDERWATER != 0 {
            *self.underwater.range(..=item & !UNDERWATER).next_back().expect("no underwater span").1
        } else {
            self.ev_span[item as usize]
        };
        (span, item - self.spans[span as usize].item)
    }

    /// Returns the place `off` characters into the span at `at`, where `off` may be the span's
    /// whole length: the end of one span is the start of the next.
    fn place_in(&self, at: Cursor, off: u32) -> Place {
        if off < self.span(at).len {
            Place::In(at, off)
        } else {
            self.tree.next(at).map_or(Place::End, |next| Place::In(next, 0))
        }
    }

    /// Finds the entry holding `item` and the place just after it, or nothing and the start of
    /// the document for `NONE`.
    fn after(&self, item: u32) -> (Option<Cursor>, Place) {
        if item == NONE {
            return (None, Place::In(self.tree.first(), 0));
        }
        let (span, off) = self.locate(item);
        let at = self.tree.cursor(span);
        (Some(at), self.place_in(at, off + 1))
    }

    /// Returns the place just before `item`, or the end of the document for `NONE`.
    fn place_before(&self, item: u32) -> Place {
        if item == NONE {
            return Place::End;
        }
        let (span, off) = self.locate(item);
        Place::In(self.tree.cursor(span), off)
    }

    /// Compares two places by their position in the document.
    fn cmp(&self, a: Place, b: Place) -> Ordering {
        match (a, b) {
            (Place::End, Place::End) => Ordering::Equal,
            (Place::End, _) => Ordering::Greater,
            (_, Place::End) => Ordering::Less,
            (Place::In(x, i), Place::In(y, j)) => self.tree.cmp(x, y).then(i.cmp(&j)),
        }
    }

    // ---------------------------------------------------------------- splitting spans

    /// Splits a span after its first `at` characters. Returns the left and right parts; one
    /// of them is the span that was passed in.
    fn split(&mut self, span: u32, at: u32) -> (u32, u32) {
        let whole = self.spans[span as usize];
        debug_assert!(at > 0 && at < whole.len);
        let left = Span { len: at, ..whole };
        let right = Span { item: whole.item + at, len: whole.len - at, origin_left: whole.item + at - 1, ..whole };
        let new = self.spans.len() as u32;
        let cursor = self.tree.cursor(span);
        let events = |s: &Span| s.item as usize..(s.item + s.len) as usize;

        // Every event in the part that takes the new id has to be re-pointed, so give the
        // new id to the smaller part: each event is then re-pointed O(log) times.
        let underwater = whole.item & UNDERWATER != 0;
        if underwater || right.len <= left.len {
            self.spans[span as usize] = left;
            self.spans.push(right);
            if underwater {
                self.underwater.insert(right.item & !UNDERWATER, new);
            } else {
                self.ev_span[events(&right)].fill(new);
            }
            self.tree.split_entry(cursor, right.widths(), false);
            (span, new)
        } else {
            self.spans[span as usize] = right;
            self.spans.push(left);
            self.ev_span[events(&left)].fill(new);
            self.tree.split_entry(cursor, left.widths(), true);
            (new, span)
        }
    }

    /// Returns a span covering exactly `len` characters of `span` starting `off` in,
    /// splitting it out if `span` is longer.
    fn isolate(&mut self, mut span: u32, off: u32, len: u32) -> u32 {
        if off > 0 {
            span = self.split(span, off).1;
        }
        if len < self.spans[span as usize].len {
            span = self.split(span, len).0;
        }
        span
    }

    // ---------------------------------------------------------------- inserting

    /// Merges an insert of `len` characters at prepare-version index `pos`, made by region
    /// events `lv..lv + len`. Returns the index to insert at in the effect version.
    pub fn insert(&mut self, lv: u32, len: u32, pos: u32, goes_before: impl Fn(u32, u32) -> bool) -> u32 {
        let (origin_left, left, after_left) = if pos == 0 {
            (NONE, None, Place::In(self.tree.first(), 0))
        } else {
            let (at, off) = self.tree.char_idx_to_entry(View::Prepare, pos - 1);
            (self.span(at).item + off, Some(at), self.place_in(at, off + 1))
        };

        // The first thing to our right that already existed, deleted or not. Anything
        // before it is a concurrent insert that we have to be ordered against.
        let mut concurrent = false;
        let origin_right = match after_left {
            Place::End => NONE,
            Place::In(at, off) if off > 0 => self.span(at).item + off,
            Place::In(mut at, _) => loop {
                let span = self.span(at);
                if span.state != NOT_INSERTED_YET {
                    break span.item;
                }
                concurrent = true;
                match self.tree.next(at) {
                    Some(next) => at = next,
                    None => break NONE,
                }
            },
        };

        let place = if concurrent { self.integrate(after_left, lv, origin_left, origin_right, &goes_before) } else { after_left };
        self.insert_span(place, left.filter(|_| place == after_left), lv, len, origin_left, origin_right)
    }

    /// Merges an insert whose neighbours are already known, so that no prepare version is
    /// needed: the origins are those [`origins`](Self::origins) reported when the event was
    /// first merged.
    pub fn insert_by_id(&mut self, lv: u32, len: u32, origin_left: u32, origin_right: u32, goes_before: impl Fn(u32, u32) -> bool) -> u32 {
        let (left, after_left) = self.after(origin_left);
        let next = match after_left {
            Place::End => NONE,
            Place::In(at, off) => self.span(at).item + off,
        };
        // Everything between the two origins was inserted concurrently with this event.
        let place = if next != origin_right { self.integrate(after_left, lv, origin_left, origin_right, &goes_before) } else { after_left };
        self.insert_span(place, left.filter(|_| place == after_left), lv, len, origin_left, origin_right)
    }

    /// Finds where a new insert goes among the concurrent inserts that start at `start`,
    /// the place just after its left origin.
    fn integrate(&mut self, start: Place, lv: u32, origin_left: u32, origin_right: u32, goes_before: &impl Fn(u32, u32) -> bool) -> Place {
        // Spans passed over are stamped, so that "is this item in the stretch already
        // scanned?" is one array read rather than a comparison of positions.
        self.scan_stamp += 1;
        let stamp = self.scan_stamp;
        self.scanned.resize(self.spans.len(), 0);
        let Place::In(first, first_off) = start else { return start };
        let first = self.tree.id(first);

        let mut here = start;
        // Where to land if the scan ends without settling it: before the first of a series
        // of inserts that share our left origin but reach less far to the right.
        let mut fallback = None;
        let landed = loop {
            let Place::In(at, off) = here else { break here };
            let id = self.tree.id(at);
            let span = self.spans[id as usize];
            let other = span.item + off;
            if other == origin_right {
                break here;
            }
            self.scanned[id as usize] = stamp;
            let other_left = if off == 0 { span.origin_left } else { other - 1 };

            // Where the other insert's left origin lies relative to ours. If it is not the
            // same item, it is either inside the stretch scanned so far (the other insert
            // descends from something we have passed) or to the left of it.
            let order = if other_left == origin_left {
                Ordering::Equal
            } else if other_left == NONE {
                Ordering::Less
            } else {
                let (left_span, left_off) = self.locate(other_left);
                if self.scanned[left_span as usize] == stamp && (left_span != first || left_off >= first_off) {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            };
            debug_assert_eq!(order, self.cmp(self.after(other_left).1, start));

            match order {
                Ordering::Less => break here,
                Ordering::Greater => {}
                Ordering::Equal if span.origin_right == origin_right => {
                    if goes_before(lv, other) {
                        break here;
                    }
                    fallback = None;
                }
                Ordering::Equal => {
                    let ends_sooner = self.cmp(self.place_before(span.origin_right), self.place_before(origin_right)) == Ordering::Less;
                    fallback = if ends_sooner { fallback.or(Some(here)) } else { None };
                }
            }
            here = self.tree.next(at).map_or(Place::End, |next| Place::In(next, 0));
        };
        fallback.unwrap_or(landed)
    }

    /// Adds the characters of events `lv..lv + len` at `place`, and returns their index in
    /// the effect version. `left` is the entry holding the left origin, given only when
    /// `place` is still the place just after that origin.
    fn insert_span(&mut self, place: Place, left: Option<Cursor>, lv: u32, len: u32, origin_left: u32, origin_right: u32) -> u32 {
        // Typing on from the character before: extend its span instead of adding one.
        if let Some(at) = left {
            let id = self.tree.id(at);
            let prev = &mut self.spans[id as usize];
            let follows = prev.item.wrapping_add(prev.len) == lv && origin_left == lv.wrapping_sub(1);
            if follows && prev.origin_right == origin_right && prev.state == INSERTED && !prev.ever_deleted {
                prev.len += len;
                let (widths, old_len) = (prev.widths(), prev.len - len);
                self.ev_span[lv as usize..(lv + len) as usize].fill(id);
                return self.tree.update(at, widths) + old_len;
            }
        }

        let before = match place {
            Place::End => None,
            Place::In(at, 0) => Some(at),
            Place::In(at, off) => {
                let right = self.split(self.tree.id(at), off).1;
                Some(self.tree.cursor(right))
            }
        };
        let span = Span { item: lv, len, origin_left, origin_right, state: INSERTED, ever_deleted: false };
        let (id, rank) = self.tree.insert(before, span.widths());
        debug_assert_eq!(id as usize, self.spans.len());
        self.spans.push(span);
        self.ev_span[lv as usize..(lv + len) as usize].fill(id);
        rank
    }

    /// Returns the left and right origins of the character inserted by region event `lv`.
    pub fn origins(&self, lv: u32) -> [u32; 2] {
        let span = &self.spans[self.ev_span[lv as usize] as usize];
        [if lv == span.item { span.origin_left } else { lv - 1 }, span.origin_right]
    }

    // ---------------------------------------------------------------- deleting

    /// Merges as much as it can of a delete of `len` characters made by region events
    /// starting at `lv`: the prepare-version range `pos..pos + len`, removed first-to-last
    /// (`fwd`) or last-to-first.
    pub fn delete(&mut self, lv: u32, pos: u32, len: u32, fwd: bool) -> (u32, Option<u32>) {
        let (span, off, n) = if fwd {
            let (at, off) = self.tree.char_idx_to_entry(View::Prepare, pos);
            (self.tree.id(at), off, len.min(self.span(at).len - off))
        } else {
            let (at, off) = self.tree.char_idx_to_entry(View::Prepare, pos + len - 1);
            let n = len.min(off + 1);
            (self.tree.id(at), off + 1 - n, n)
        };
        let target = self.isolate(span, off, n);
        debug_assert_eq!(self.spans[target as usize].state, INSERTED);
        let item = self.spans[target as usize].item;
        for i in 0..n {
            self.ev_target[(lv + i) as usize] = if fwd { item + i } else { item + n - 1 - i };
        }
        (n, self.mark_deleted(target))
    }

    /// Merges deletes whose targets are already known by id: region event `lv + i` deletes
    /// the `i`th of `targets`, as [`target`](Self::target) reported when the events were
    /// first merged.
    pub fn delete_by_id(&mut self, lv: u32, targets: impl ExactSizeIterator<Item = u32>, mut removed: impl FnMut(u32, u32)) {
        let end = lv + targets.len() as u32;
        for (i, target) in targets.enumerate() {
            self.ev_target[lv as usize + i] = target;
        }
        let mut lv = lv;
        while lv < end {
            let (events, mut item) = self.target_run(lv, end);
            let stop = item + events;
            while item < stop {
                let (span, off) = self.locate(item);
                let n = (stop - item).min(self.spans[span as usize].len - off);
                let piece = self.isolate(span, off, n);
                if let Some(at) = self.mark_deleted(piece) {
                    removed(at, n);
                }
                item += n;
            }
            lv += events;
        }
    }

    /// Counts one more delete against a span. Returns its index in the effect version if
    /// this is the first.
    fn mark_deleted(&mut self, id: u32) -> Option<u32> {
        let span = &mut self.spans[id as usize];
        let (first, before) = (!span.ever_deleted, span.widths());
        span.state += 1;
        span.ever_deleted = true;
        let widths = span.widths();
        let at = self.tree.cursor(id);
        if first {
            Some(self.tree.update(at, widths))
        } else {
            if widths != before {
                self.tree.set_widths(at, widths);
            }
            None
        }
    }

    /// Returns the item deleted by region event `lv`.
    pub fn target(&self, lv: u32) -> u32 {
        self.ev_target[lv as usize]
    }

    /// Finds the longest stretch of delete events from `lv` (stopping before `end`) whose targets
    /// are consecutive items, forwards or backwards.
    fn target_run(&self, lv: u32, end: u32) -> (u32, u32) {
        let first = self.ev_target[lv as usize];
        let run = |step: u32| {
            let mut n = 1;
            while lv + n < end
                && self.ev_span[(lv + n) as usize] == NONE
                && self.ev_target[(lv + n) as usize] == first.wrapping_add(step.wrapping_mul(n))
            {
                n += 1;
            }
            n
        };
        let (up, down) = (run(1), run(1u32.wrapping_neg()));
        if up >= down {
            (up, first)
        } else {
            (down, first + 1 - down)
        }
    }

    // ---------------------------------------------------------------- moving the prepare version

    /// Takes region events out of the prepare version.
    pub fn retreat(&mut self, events: Range<u32>) {
        self.shift(events, 1u32.wrapping_neg());
    }

    /// Puts region events back into the prepare version.
    pub fn advance(&mut self, events: Range<u32>) {
        self.shift(events, 1);
    }

    /// Moves region events into or out of the prepare version, span by span.
    fn shift(&mut self, events: Range<u32>, delta: u32) {
        let mut lv = events.start;
        while lv < events.end {
            let id = self.ev_span[lv as usize];
            if id != NONE {
                // Inserts: every span, or part of one, that these events created.
                let span = &self.spans[id as usize];
                let (off, end) = (lv - span.item, events.end.min(span.item + span.len));
                let piece = self.isolate(id, off, end - lv);
                self.shift_span(piece, delta);
                lv = end;
            } else {
                let (n, mut item) = self.target_run(lv, events.end);
                let stop = item + n;
                while item < stop {
                    let (span, off) = self.locate(item);
                    let len = (stop - item).min(self.spans[span as usize].len - off);
                    let piece = self.isolate(span, off, len);
                    self.shift_span(piece, delta);
                    item += len;
                }
                lv += n;
            }
        }
    }

    /// Changes one span's state by `delta` and fixes its widths in the tree.
    fn shift_span(&mut self, id: u32, delta: u32) {
        let span = &mut self.spans[id as usize];
        let before = span.widths();
        span.state = span.state.wrapping_add(delta);
        let widths = span.widths();
        if widths != before {
            self.tree.set_widths(self.tree.cursor(id), widths);
        }
    }
}
