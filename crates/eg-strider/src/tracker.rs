use crate::oplog::OpKind;
use std::cmp::Ordering;
use std::ops::Range;
use strider_graph::EventIndex;
use strider_tree::{Cursor, EntryId, PrepareEffectTree, View, Widths};

/// A character the tracker knows: the region event that inserted it, or one of the document as
/// the region began.
pub(crate) type ItemId = u32;

/// No item: the start of the document as a left origin, the end as a right origin.
pub(crate) const NONE: ItemId = u32::MAX;

/// Marks a character of the document as the region began ("underwater"): `UNDERWATER |
/// position`.
const UNDERWATER: ItemId = 1 << 31;
/// The placeholder for the pre-region document. Its real length is not needed.
const UNDERWATER_LEN: u32 = 0x7fff_0000;

// A span's state in the prepare version. Each delete of an inserted character adds one.
const NOT_INSERTED_YET: u32 = 0;
const INSERTED: u32 = 1;

/// Characters of the pre-region document per entry of the index that finds underwater spans.
/// Tiny in debug builds, so that tests cross pages.
const PAGE: u32 = if cfg!(debug_assertions) { 4 } else { 64 };

/// Tree node size. Tiny in debug builds, so that tests split nodes.
const NODE: usize = if cfg!(debug_assertions) { 4 } else { 32 };

#[derive(Clone, Copy)]
struct Span {
    /// The first character; the rest follow consecutively.
    item: ItemId,
    len: u32,
    /// The item left of the first character when it was inserted. Each later character's left
    /// origin is the one before it.
    origin_left: ItemId,
    /// The first item to the right that existed when these were inserted.
    origin_right: ItemId,
    state: u32,
    ever_deleted: bool,
}

/// A stretch of delete events whose targets are consecutive items: the first event's is the
/// lowest, or the highest if `descending`.
#[derive(Clone, Copy)]
pub(crate) struct Targets {
    /// The first of the events.
    pub first_event: EventIndex,
    pub len: u32,
    /// The lowest item deleted.
    pub item: ItemId,
    pub descending: bool,
}

impl Targets {
    /// Returns the item that the first event deletes.
    pub fn first(&self) -> ItemId {
        if self.descending {
            self.item + self.len - 1
        } else {
            self.item
        }
    }

    /// Returns the part of the stretch from `event`, at most `len` events long.
    fn from(&self, event: EventIndex, len: u32) -> Targets {
        let (skip, len) = (event - self.first_event, len.min(self.first_event + self.len - event));
        let item = if self.descending { self.item + self.len - skip - len } else { self.item + skip };
        Targets { first_event: event, len, item, descending: self.descending }
    }
}

impl Span {
    /// Returns the span's width in the prepare and effect versions.
    fn widths(&self) -> Widths {
        Widths { prepare: if self.state == INSERTED { self.len } else { 0 }, effect: if self.ever_deleted { 0 } else { self.len } }
    }
}

/// A place between two characters.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Place {
    /// This many characters into the span at this entry, fewer than its length.
    In(Cursor, u32),
    End,
}

/// The temporary CRDT state for merging one region, as in Eg-walker: the region's characters as
/// spans with left and right origins, as in Yjs or Fugue, in a B+ tree measured in the prepare
/// and effect versions. Merging an event finds its position in the prepare version and reads it
/// off in the effect version.
pub(crate) struct Tracker {
    /// The spans in document order, measured in both versions.
    tree: PrepareEffectTree<NODE>,
    /// By tree entry id.
    spans: Vec<Span>,
    /// Per region event: for an insert, the span holding its character; for a delete, its index
    /// in `deleted`.
    by_event: Vec<u32>,
    /// What the delete events deleted, a stretch of consecutive items at a time.
    deleted: Vec<Targets>,
    /// Per page of the pre-region document, the underwater span holding its first character.
    pages: Vec<EntryId>,
    /// Per span, the `scan_stamp` of the last `integrate` scan over it.
    scanned: Vec<u32>,
    scan_stamp: u32,
}

impl Default for Tracker {
    fn default() -> Self {
        Tracker {
            tree: PrepareEffectTree::new(Widths::default()),
            spans: Vec::new(),
            by_event: Vec::new(),
            deleted: Vec::new(),
            pages: Vec::new(),
            scanned: Vec::new(),
            scan_stamp: 0,
        }
    }
}

impl Tracker {
    /// Resets to one placeholder for the current document, `len` characters long, for a region
    /// of `events` events.
    pub fn begin_region(&mut self, events: usize, len: u32) {
        let placeholder = Span { item: UNDERWATER, len: UNDERWATER_LEN, origin_left: NONE, origin_right: NONE, state: INSERTED, ever_deleted: false };
        self.tree.reset(placeholder.widths());
        self.spans.clear();
        self.spans.push(placeholder);
        self.pages.clear();
        self.pages.resize((len / PAGE) as usize + 1, 0);
        self.scanned.clear();
        self.scan_stamp = 0;
        self.by_event.clear();
        self.by_event.resize(events, NONE);
        self.deleted.clear();
    }

    // ---------------------------------------------------------------- finding things

    /// Returns the span stored at a tree entry.
    fn span(&self, at: Cursor) -> &Span {
        &self.spans[self.tree.id(at) as usize]
    }

    /// Finds the span holding `item`, and the offset into it.
    fn locate(&self, item: ItemId) -> (EntryId, u32) {
        let span = if item & UNDERWATER != 0 { self.underwater(item) } else { self.by_event[item as usize] };
        (span, item - self.spans[span as usize].item)
    }

    /// Finds the span holding an underwater item, starting from the span that holds its page's
    /// first character.
    fn underwater(&self, item: ItemId) -> EntryId {
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

    /// Returns the place `offset` characters into the span at `at`. The span's full length is
    /// the start of the next span.
    fn place_in(&self, at: Cursor, offset: u32) -> Place {
        if offset < self.span(at).len {
            Place::In(at, offset)
        } else {
            self.tree.next(at).map_or(Place::End, |next| Place::In(next, 0))
        }
    }

    /// Finds the entry holding `item` and the place just after it; for `NONE`, nothing and the
    /// start of the document.
    fn after(&self, item: ItemId) -> (Option<Cursor>, Place) {
        if item == NONE {
            return (None, Place::In(self.tree.first(), 0));
        }
        let (span, offset) = self.locate(item);
        let at = self.tree.cursor(span);
        (Some(at), self.place_in(at, offset + 1))
    }

    /// Returns the place just before `item`, or the end of the document for `NONE`.
    fn place_before(&self, item: ItemId) -> Place {
        if item == NONE {
            return Place::End;
        }
        let (span, offset) = self.locate(item);
        Place::In(self.tree.cursor(span), offset)
    }

    /// Compares two places' positions in the document.
    fn cmp(&self, a: Place, b: Place) -> Ordering {
        match (a, b) {
            (Place::End, Place::End) => Ordering::Equal,
            (Place::End, _) => Ordering::Greater,
            (_, Place::End) => Ordering::Less,
            (Place::In(x, i), Place::In(y, j)) => self.tree.cmp(x, y).then(i.cmp(&j)),
        }
    }

    // ---------------------------------------------------------------- splitting spans

    /// Splits the span at `at` after `offset` characters. Returns both parts; one keeps the
    /// span's id.
    fn split(&mut self, at: Cursor, offset: u32) -> [Cursor; 2] {
        let span = self.tree.id(at);
        let whole = self.spans[span as usize];
        debug_assert!(offset > 0 && offset < whole.len);
        let left = Span { len: offset, ..whole };
        let right = Span { item: whole.item + offset, len: whole.len - offset, origin_left: whole.item + offset - 1, ..whole };
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

    /// Splits the span at `at` so that one span covers exactly `len` characters from `offset`,
    /// and returns it.
    fn isolate(&mut self, mut at: Cursor, offset: u32, len: u32) -> Cursor {
        if offset > 0 {
            at = self.split(at, offset)[1];
        }
        if len < self.span(at).len {
            at = self.split(at, len)[0];
        }
        at
    }

    // ---------------------------------------------------------------- inserting

    /// Merges an insert of `len` characters at prepare index `pos`, made by region events
    /// `event..event + len`. Returns its effect index.
    pub fn insert(&mut self, event: EventIndex, len: u32, pos: u32, goes_before: impl Fn(EventIndex, EventIndex) -> bool) -> u32 {
        // `index`: where the place just after the left origin is in the effect version, which
        // finding the origin gives. The origin's own character is there unless it was deleted.
        let (origin_left, left, after_left, index) = if pos == 0 {
            (NONE, None, Place::In(self.tree.first(), 0), 0)
        } else {
            let (at, offset, start) = self.tree.locate(View::Prepare, pos - 1);
            let span = self.span(at);
            (span.item + offset, Some(at), self.place_in(at, offset + 1), start + if span.ever_deleted { 0 } else { offset + 1 })
        };

        // The first thing to our right that already existed, deleted or not. Anything
        // before it is a concurrent insert that we have to be ordered against.
        let mut concurrent = false;
        let origin_right = match after_left {
            Place::End => NONE,
            Place::In(at, offset) if offset > 0 => self.span(at).item + offset,
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

        // Among concurrent inserts the place may be further along, and is found by climbing.
        let place = if concurrent { self.integrate(after_left, event, origin_left, origin_right, &goes_before) } else { after_left };
        self.insert_span(place, left.filter(|_| place == after_left), event, len, origin_left, origin_right, (!concurrent).then_some(index))
    }

    /// Merges an insert whose origins are known from [`origins`](Self::origins), needing no
    /// prepare version.
    pub fn insert_by_id(
        &mut self,
        event: EventIndex,
        len: u32,
        origin_left: ItemId,
        origin_right: ItemId,
        goes_before: impl Fn(EventIndex, EventIndex) -> bool,
    ) -> u32 {
        let (left, after_left) = self.after(origin_left);
        let next = match after_left {
            Place::End => NONE,
            Place::In(at, offset) => self.span(at).item + offset,
        };
        // Everything between the two origins was inserted concurrently with this event.
        let place = if next != origin_right { self.integrate(after_left, event, origin_left, origin_right, &goes_before) } else { after_left };
        self.insert_span(place, left.filter(|_| place == after_left), event, len, origin_left, origin_right, None)
    }

    /// Finds where a new insert goes among the concurrent inserts starting at `start`, just
    /// after its left origin.
    fn integrate(
        &mut self,
        start: Place,
        event: EventIndex,
        origin_left: ItemId,
        origin_right: ItemId,
        goes_before: &impl Fn(EventIndex, EventIndex) -> bool,
    ) -> Place {
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
            let Place::In(at, offset) = here else { break here };
            let id = self.tree.id(at);
            let span = self.spans[id as usize];
            let other = span.item + offset;
            if other == origin_right {
                break here;
            }
            self.scanned[id as usize] = stamp;
            let other_left = if offset == 0 { span.origin_left } else { other - 1 };

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
                    if goes_before(event, other) {
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

    /// Adds the characters of events `event..event + len` at `place`, returning their effect
    /// index: `index` if given, else found by climbing the tree. `left` is the entry holding
    /// the left origin, given only while `place` is right after it.
    #[allow(clippy::too_many_arguments)]
    fn insert_span(
        &mut self,
        place: Place,
        left: Option<Cursor>,
        event: EventIndex,
        len: u32,
        origin_left: ItemId,
        origin_right: ItemId,
        index: Option<u32>,
    ) -> u32 {
        // Typing on from the character before: extend its span instead of adding one.
        if let Some(at) = left {
            let id = self.tree.id(at);
            let prev = &mut self.spans[id as usize];
            let follows = prev.item.wrapping_add(prev.len) == event && origin_left == event.wrapping_sub(1);
            if follows && prev.origin_right == origin_right && prev.state == INSERTED && !prev.ever_deleted {
                prev.len += len;
                let (widths, old_len) = (prev.widths(), prev.len - len);
                self.by_event[event as usize..(event + len) as usize].fill(id);
                return match index {
                    Some(index) => {
                        self.tree.set_widths(at, widths);
                        index
                    }
                    None => self.tree.update(at, widths) + old_len,
                };
            }
        }

        let before = match place {
            Place::End => None,
            Place::In(at, 0) => Some(at),
            Place::In(at, offset) => Some(self.split(at, offset)[1]),
        };
        let span = Span { item: event, len, origin_left, origin_right, state: INSERTED, ever_deleted: false };
        let (id, index) = match index {
            Some(index) => (self.tree.add(before, span.widths()), index),
            None => self.tree.insert(before, span.widths()),
        };
        debug_assert_eq!(id as usize, self.spans.len());
        self.spans.push(span);
        self.by_event[event as usize..(event + len) as usize].fill(id);
        index
    }

    /// Returns the left and right origins of the character inserted by region event `event`.
    pub fn origins(&self, event: EventIndex) -> [ItemId; 2] {
        let span = &self.spans[self.by_event[event as usize] as usize];
        [if event == span.item { span.origin_left } else { event - 1 }, span.origin_right]
    }

    // ---------------------------------------------------------------- deleting

    /// Merges a delete by region events `event..event + len` of the prepare range `pos..pos +
    /// len`, removed first to last if `forward`. Calls `removed` with the effect index and
    /// length of each run still there.
    pub fn delete(&mut self, event: EventIndex, pos: u32, len: u32, forward: bool, mut removed: impl FnMut(u32, u32)) {
        let (mut at, mut offset, start) = self.tree.locate(View::Prepare, pos);
        // Where the characters to delete next are in the effect version, if they are still
        // there. Finding the first of them gave it; from there it only moves past what is in
        // the effect version and not in the prepare version.
        let mut index = start + if self.span(at).ever_deleted { 0 } else { offset };
        let mut done = 0;
        loop {
            // The span holds characters `done..done + n` of the range.
            let n = (len - done).min(self.span(at).len - offset);
            at = self.isolate(at, offset, n);
            debug_assert_eq!(self.span(at).state, INSERTED);
            // The events that deleted these characters: the next ones, or with a backspace
            // the ones before those already accounted for.
            let first = if forward { event + done } else { event + len - done - n };
            self.note_deleted(Targets { first_event: first, len: n, item: self.span(at).item, descending: !forward });
            if self.mark_deleted(at, Some(index)).is_some() {
                removed(index, n);
            }
            done += n;
            if done == len {
                return;
            }
            // The range runs on into the next span that is in the prepare version.
            at = self.tree.next(at).expect("a delete stays inside the document");
            while self.span(at).state != INSERTED {
                index += self.span(at).widths().effect;
                at = self.tree.next(at).expect("a delete stays inside the document");
            }
            offset = 0;
        }
    }

    /// Records what a stretch of delete events deleted, joining the stretch before when it is
    /// the same delete carrying on.
    fn note_deleted(&mut self, targets: Targets) {
        let Targets { first_event: event, len, item, descending } = targets;
        match self.deleted.last_mut() {
            // A backspace reaches the next span with the events before those it has used.
            Some(last)
                if last.descending == descending
                    && last.item + last.len == item
                    && (if descending { event + len == last.first_event } else { last.first_event + last.len == event }) =>
            {
                last.len += len;
                last.first_event = last.first_event.min(event);
            }
            _ => self.deleted.push(targets),
        }
        self.by_event[event as usize..(event + len) as usize].fill(self.deleted.len() as u32 - 1);
    }

    /// Merges deletes whose targets are known from [`targets`](Self::targets).
    pub fn delete_by_id(&mut self, targets: Targets, mut removed: impl FnMut(u32, u32)) {
        self.note_deleted(targets);
        let (mut item, stop) = (targets.item, targets.item + targets.len);
        while item < stop {
            let (span, offset) = self.locate(item);
            let n = (stop - item).min(self.spans[span as usize].len - offset);
            let at = self.isolate(self.tree.cursor(span), offset, n);
            if let Some(index) = self.mark_deleted(at, None) {
                removed(index, n);
            }
            item += n;
        }
    }

    /// Counts one more delete of the span at `at`. Returns its effect index if this is the
    /// first: `index` if given, else found by climbing the tree.
    fn mark_deleted(&mut self, at: Cursor, index: Option<u32>) -> Option<u32> {
        let span = &mut self.spans[self.tree.id(at) as usize];
        let (first, before) = (!span.ever_deleted, span.widths());
        span.state += 1;
        span.ever_deleted = true;
        let widths = span.widths();
        match index {
            None if first => return Some(self.tree.update(at, widths)),
            _ if widths != before => self.tree.set_widths(at, widths),
            _ => {}
        }
        index.filter(|_| first)
    }

    /// Returns what the delete events from `event` deleted, up to `end` or until the targets
    /// stop being consecutive.
    pub fn targets(&self, event: EventIndex, end: EventIndex) -> Targets {
        self.deleted[self.by_event[event as usize] as usize].from(event, end - event)
    }

    // ---------------------------------------------------------------- moving the prepare version

    /// Takes region events, all inserts or all deletes, out of the prepare version.
    pub fn retreat(&mut self, kind: OpKind, events: Range<EventIndex>) {
        self.shift(kind, events, 1u32.wrapping_neg());
    }

    /// Puts region events, all inserts or all deletes, back into the prepare version.
    pub fn advance(&mut self, kind: OpKind, events: Range<EventIndex>) {
        self.shift(kind, events, 1);
    }

    /// Moves region events into or out of the prepare version, span by span.
    fn shift(&mut self, kind: OpKind, events: Range<EventIndex>, delta: u32) {
        let mut event = events.start;
        while event < events.end {
            match kind {
                // Every span, or part of one, that these events created.
                OpKind::Ins => {
                    let id = self.by_event[event as usize];
                    let span = &self.spans[id as usize];
                    let (offset, end) = (event - span.item, events.end.min(span.item + span.len));
                    let at = self.isolate(self.tree.cursor(id), offset, end - event);
                    self.shift_span(at, delta);
                    event = end;
                }
                OpKind::Del => {
                    let Targets { len: n, mut item, .. } = self.targets(event, events.end);
                    let stop = item + n;
                    while item < stop {
                        let (span, offset) = self.locate(item);
                        let len = (stop - item).min(self.spans[span as usize].len - offset);
                        let at = self.isolate(self.tree.cursor(span), offset, len);
                        self.shift_span(at, delta);
                        item += len;
                    }
                    event += n;
                }
            }
        }
    }

    /// Changes the state of the span at `at` by `delta`, updating its widths.
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
