//! Builds a document's final text from the edits made to it. A replay only wants the text
//! at the end and knows every edit beforehand, so instead of applying the edits to a rope
//! it takes them back from last to first. What it tracks is a piece table over the final
//! text: each piece of the document at that moment either maps to a place in the final text
//! or was deleted later. Taking back an insert copies whichever of its characters survive
//! straight to their final place.

mod pieces;

use pieces::PieceTable;

/// A document being taken back from its final state to empty, one edit at a time, last edit
/// first. Positions and lengths count characters.
pub struct Rewind {
    pieces: PieceTable,
    /// Length of the document with the edits taken back so far undone.
    len: u32,
    /// The final text as it is assembled: one byte per character, since the characters are
    /// placed out of order. A character that is not ASCII has [`WIDE`] here and is in `wide`.
    slots: Vec<u8>,
    wide: Wide,
}

/// In place of a character that does not fit in a byte.
const WIDE: u8 = 0x80;

/// How many positions of the final text share a page of [`Wide`]. Tiny in debug builds, so
/// that tests on short texts use several pages.
const PAGE: usize = if cfg!(debug_assertions) { 4 } else { 256 };

/// The characters of the final text that are not ASCII, by position. Text is mostly ASCII
/// with the odd character that is not, so room is only made for pages that have some.
#[derive(Default)]
struct Wide {
    /// For each page of positions, where its characters start in `chars`, if it has any.
    pages: Vec<Option<u32>>,
    chars: Vec<char>,
}

impl Wide {
    /// Records the character at position `at`.
    #[cold]
    fn set(&mut self, at: usize, ch: char) {
        if self.pages.len() <= at / PAGE {
            self.pages.resize(at / PAGE + 1, None);
        }
        let start = *self.pages[at / PAGE].get_or_insert_with(|| {
            self.chars.resize(self.chars.len() + PAGE, '\0');
            (self.chars.len() - PAGE) as u32
        });
        self.chars[start as usize + at % PAGE] = ch;
    }

    /// Returns the character recorded at position `at`.
    fn get(&self, at: usize) -> char {
        let start = self.pages[at / PAGE].expect("a character was recorded on this page");
        self.chars[start as usize + at % PAGE]
    }
}

impl Rewind {
    /// Starts from the final document, which is `len` characters long.
    pub fn new(len: u32) -> Self {
        Rewind { pieces: PieceTable::new(len), len, slots: vec![0; len as usize], wide: Wide::default() }
    }

    /// Takes back an insert that put `text`, `len` characters long, at `pos`.
    pub fn undo_insert(&mut self, pos: u32, text: &str, len: u32) {
        assert!(pos.checked_add(len).is_some_and(|end| end <= self.len), "insert was not within the document");
        // Debug builds only: counting takes as long as the text.
        debug_assert_eq!(text.chars().count(), len as usize);
        if len == 0 {
            return;
        }
        self.len -= len;
        let (slots, wide) = (&mut self.slots, &mut self.wide);
        if text.len() == len as usize {
            // All ASCII: characters and bytes coincide.
            self.pieces.remove(pos, len, |at, skip, n| {
                let (at, skip, n) = (at as usize, skip as usize, n as usize);
                slots[at..at + n].copy_from_slice(&text.as_bytes()[skip..skip + n]);
            });
        } else {
            // The survivors come in the order they have in `text`, so one pass over it
            // reaches them all. `passed` counts the characters gone by.
            let (mut rest, mut passed) = (text.chars(), 0);
            self.pieces.remove(pos, len, |at, skip, n| {
                if skip > passed {
                    rest.nth((skip - passed - 1) as usize);
                }
                for (at, ch) in (at as usize..).zip(rest.by_ref().take(n as usize)) {
                    slots[at] = if ch.is_ascii() { ch as u8 } else { WIDE };
                    if !ch.is_ascii() {
                        wide.set(at, ch);
                    }
                }
                passed = skip + n;
            });
        }
    }

    /// Takes back a removal of `len` characters from `pos`.
    pub fn undo_remove(&mut self, pos: u32, len: u32) {
        assert!(pos <= self.len, "removal was not within the document");
        if len == 0 {
            return;
        }
        self.len = self.len.checked_add(len).expect("document too long");
        self.pieces.insert_deleted(pos, len);
    }

    /// Returns the final text.
    pub fn finish(self) -> String {
        assert_eq!(self.len, 0, "edits left to take back");
        if self.wide.chars.is_empty() {
            return String::from_utf8(self.slots).expect("ASCII");
        }
        // Mostly ASCII in practice: whole stretches of it are copied at once.
        let mut bytes = Vec::with_capacity(self.slots.len() + self.wide.chars.len());
        for (i, stretch) in self.slots.chunks(16).enumerate() {
            if stretch.is_ascii() {
                bytes.extend_from_slice(stretch);
            } else {
                for (at, &byte) in (i * 16..).zip(stretch) {
                    let ch = if byte == WIDE { self.wide.get(at) } else { byte as char };
                    bytes.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes());
                }
            }
        }
        String::from_utf8(bytes).expect("encoded from characters")
    }
}
