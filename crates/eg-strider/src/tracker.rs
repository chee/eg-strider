//! The tracker: the temporary CRDT state for merging one region, as in Eg-walker. It holds
//! the region's inserted text as spans (the items of a sequence CRDT such as Yjs or Fugue,
//! with left and right origins) in a B+ tree that measures them in two versions: the one an
//! event was made in (prepare) and the merged document (effect). Merging an event means
//! finding its position in the first and reading it off in the second. A flat array maps each
//! event to its span or its delete target, for moving the prepare version.

use crate::oplog::OpKind;
use std::cmp::Ordering;
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

/// How many characters of the pre-region document share an entry of the index that finds
/// underwater spans. Tiny in debug builds, so that tests on small documents cross pages.
const PAGE: u32 = if cfg!(debug_assertions) { 4 } else { 64 };

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
    /// Per region event. For an insert, the span holding the character it inserted; for a
    /// delete, the item it deleted.
    by_event: Vec<u32>,
    /// For each page of the document as it stood when the region began, the underwater span
    /// that holds the page's first character.
    pages: Vec<u32>,
    /// Per span: the `scan_stamp` of the last `integrate` scan that passed over it.
    scanned: Vec<u32>,
    scan_stamp: u32,
}

impl Default for Tracker {
    fn default() -> Self {
        Tracker {
            tree: PrepareEffectTree::new(Widths::default()),
            spans: Vec::new(),
            by_event: Vec::new(),
            pages: Vec::new(),
            scanned: Vec::new(),
            scan_stamp: 0,
        }
    }
}

impl Tracker {
    /// Resets to a single placeholder for the document as it stands, `len` characters long,
    /// ready for a region of `events` events.
    pub fn begin_region(&mut self, events: usize, len: u32) {
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
        self.pages.clear();
        self.pages.resize((len / PAGE) as usize + 1, 0);
        self.scanned.clear();
        self.scan_stamp = 0;
        self.by_event.clear();
        self.by_event.resize(events, NONE);
    }

    // ---------------------------------------------------------------- finding things

    /// Returns the span stored at a tree entry.
    fn span(&self, at: Cursor) -> &Span {
        &self.spans[self.tree.id(at) as usize]
    }

    /// Finds the span holding `item`, and how far into it.
    fn locate(&self, item: u32) -> (u32, u32) {
        let span = if item & UNDERWATER != 0 { self.underwater(item) } else { self.by_event[item as usize] };
        (span, item - self.spans[span as usize].item)
    }

    /// Finds the span holding an underwater item: the one holding the first character of
    /// its page, or one further along the document.
    fn underwater(&self, item: u32) -> u32 {
        let holds = |id: u32| {
            let span = &self.spans[id as usize];
            span.item & UNDERWATER != 0 && item.wrapping_sub(span.item) < span.len
        };
        let mut id = self.pages[((item & !UNDERWATER) / PAGE) as usize];
        if holds(id) {
            return id;
        }
        let mut at = self.tree.cursor(id);
        loop {
            at = self.tree.next(at).expect("an underwater item is in some span");
            id = self.tree.id(at);
            if holds(id) {
                return id;
            }
        }
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

    /// Splits the span at `at` after its first `off` characters. Returns where the left and
    /// right parts are; one of them keeps the span's id.
    fn split(&mut self, at: Cursor, off: u32) -> [Cursor; 2] {
        let span = self.tree.id(at);
        let whole = self.spans[span as usize];
        debug_assert!(off > 0 && off < whole.len);
        let left = Span { len: off, ..whole };
        let right = Span { item: whole.item + off, len: whole.len - off, origin_left: whole.item + off - 1, ..whole };
        let new = self.spans.len() as u32;
        let events = |s: &Span| s.item as usize..(s.item + s.len) as usize;

        // Every event in the part that takes the new id has to be re-pointed, so give the
        // new id to the smaller part: each event is then re-pointed O(log) times. Likewise
        // every page that begins in an underwater part.
        let underwater = whole.item & UNDERWATER != 0;
        let index = if underwater { &mut self.pages } else { &mut self.by_event };
        let pointed = |s: &Span| {
            if underwater {
                let page = |pos: u32| ((pos & !UNDERWATER).div_ceil(PAGE) as usize).min(index.len());
                page(s.item)..page(s.item + s.len)
            } else {
                events(s)
            }
        };
        let (from_left, from_right) = (pointed(&left), pointed(&right));
        if from_right.len() <= from_left.len() {
            self.spans[span as usize] = left;
            self.spans.push(right);
            index[from_right].fill(new);
            self.tree.split_entry(at, right.widths(), false).1
        } else {
            self.spans[span as usize] = right;
            self.spans.push(left);
            index[from_left].fill(new);
            self.tree.split_entry(at, left.widths(), true).1
        }
    }

    /// Splits the span at `at` as needed for one span to cover exactly `len` of its
    /// characters, starting `off` in. Returns where that span is.
    fn isolate(&mut self, mut at: Cursor, off: u32, len: u32) -> Cursor {
        if off > 0 {
            at = self.split(at, off)[1];
        }
        if len < self.span(at).len {
            at = self.split(at, len)[0];
        }
        at
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
                self.by_event[lv as usize..(lv + len) as usize].fill(id);
                return self.tree.update(at, widths) + old_len;
            }
        }

        let before = match place {
            Place::End => None,
            Place::In(at, 0) => Some(at),
            Place::In(at, off) => Some(self.split(at, off)[1]),
        };
        let span = Span { item: lv, len, origin_left, origin_right, state: INSERTED, ever_deleted: false };
        let (id, rank) = self.tree.insert(before, span.widths());
        debug_assert_eq!(id as usize, self.spans.len());
        self.spans.push(span);
        self.by_event[lv as usize..(lv + len) as usize].fill(id);
        rank
    }

    /// Returns the left and right origins of the character inserted by region event `lv`.
    pub fn origins(&self, lv: u32) -> [u32; 2] {
        let span = &self.spans[self.by_event[lv as usize] as usize];
        [if lv == span.item { span.origin_left } else { lv - 1 }, span.origin_right]
    }

    // ---------------------------------------------------------------- deleting

    /// Merges a delete made by region events `lv..lv + len`: of the prepare-version range
    /// `pos..pos + len`, removed first-to-last (`fwd`) or last-to-first. Calls `removed` with
    /// the effect-version index and length of each run of characters that was still there.
    pub fn delete(&mut self, lv: u32, pos: u32, len: u32, fwd: bool, mut removed: impl FnMut(u32, u32)) {
        let (mut at, mut off) = self.tree.char_idx_to_entry(View::Prepare, pos);
        let mut done = 0;
        loop {
            // The span holds characters `done..done + n` of the range.
            let n = (len - done).min(self.span(at).len - off);
            at = self.isolate(at, off, n);
            debug_assert_eq!(self.span(at).state, INSERTED);
            let item = self.span(at).item;
            for i in 0..n {
                let event = if fwd { lv + done + i } else { lv + len - 1 - done - i };
                self.by_event[event as usize] = item + i;
            }
            if let Some(index) = self.mark_deleted(at) {
                removed(index, n);
            }
            done += n;
            if done == len {
                return;
            }
            // The range runs on into the next span that is in the prepare version.
            at = self.tree.next(at).expect("a delete stays inside the document");
            while self.span(at).state != INSERTED {
                at = self.tree.next(at).expect("a delete stays inside the document");
            }
            off = 0;
        }
    }

    /// Merges deletes whose targets are already known by id: region event `lv + i` deletes
    /// the `i`th of `targets`, as [`target`](Self::target) reported when the events were
    /// first merged.
    pub fn delete_by_id(&mut self, lv: u32, targets: impl ExactSizeIterator<Item = u32>, mut removed: impl FnMut(u32, u32)) {
        let end = lv + targets.len() as u32;
        for (i, target) in targets.enumerate() {
            self.by_event[lv as usize + i] = target;
        }
        let mut lv = lv;
        while lv < end {
            let (events, mut item) = self.target_run(lv, end);
            let stop = item + events;
            while item < stop {
                let (span, off) = self.locate(item);
                let n = (stop - item).min(self.spans[span as usize].len - off);
                let at = self.isolate(self.tree.cursor(span), off, n);
                if let Some(index) = self.mark_deleted(at) {
                    removed(index, n);
                }
                item += n;
            }
            lv += events;
        }
    }

    /// Counts one more delete against the span at `at`. Returns its index in the effect
    /// version if this is the first.
    fn mark_deleted(&mut self, at: Cursor) -> Option<u32> {
        let span = &mut self.spans[self.tree.id(at) as usize];
        let (first, before) = (!span.ever_deleted, span.widths());
        span.state += 1;
        span.ever_deleted = true;
        let widths = span.widths();
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
        self.by_event[lv as usize]
    }

    /// Finds the longest stretch of the delete events `lv..end` that starts at `lv` and whose
    /// targets are consecutive items, forwards or backwards.
    fn target_run(&self, lv: u32, end: u32) -> (u32, u32) {
        let first = self.by_event[lv as usize];
        let run = |step: u32| {
            let mut n = 1;
            while lv + n < end && self.by_event[(lv + n) as usize] == first.wrapping_add(step.wrapping_mul(n)) {
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

    /// Takes region events, all inserts or all deletes, out of the prepare version.
    pub fn retreat(&mut self, kind: OpKind, events: Range<u32>) {
        self.shift(kind, events, 1u32.wrapping_neg());
    }

    /// Puts region events, all inserts or all deletes, back into the prepare version.
    pub fn advance(&mut self, kind: OpKind, events: Range<u32>) {
        self.shift(kind, events, 1);
    }

    /// Moves region events into or out of the prepare version, span by span.
    fn shift(&mut self, kind: OpKind, events: Range<u32>, delta: u32) {
        let mut lv = events.start;
        while lv < events.end {
            match kind {
                // Every span, or part of one, that these events created.
                OpKind::Ins => {
                    let id = self.by_event[lv as usize];
                    let span = &self.spans[id as usize];
                    let (off, end) = (lv - span.item, events.end.min(span.item + span.len));
                    let at = self.isolate(self.tree.cursor(id), off, end - lv);
                    self.shift_span(at, delta);
                    lv = end;
                }
                OpKind::Del => {
                    let (n, mut item) = self.target_run(lv, events.end);
                    let stop = item + n;
                    while item < stop {
                        let (span, off) = self.locate(item);
                        let len = (stop - item).min(self.spans[span as usize].len - off);
                        let at = self.isolate(self.tree.cursor(span), off, len);
                        self.shift_span(at, delta);
                        item += len;
                    }
                    lv += n;
                }
            }
        }
    }

    /// Changes the state of the span at `at` by `delta` and fixes its widths in the tree.
    fn shift_span(&mut self, at: Cursor, delta: u32) {
        let span = &mut self.spans[self.tree.id(at) as usize];
        let before = span.widths();
        span.state = span.state.wrapping_add(delta);
        let widths = span.widths();
        if widths != before {
            self.tree.set_widths(at, widths);
        }
    }
}
