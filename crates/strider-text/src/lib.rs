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
    text: Text,
}

/// The final text as it is assembled: one slot per character, since the characters are
/// placed out of order. Bytes will do until something other than ASCII turns up.
enum Text {
    Ascii(Vec<u8>),
    Wide(Vec<char>),
}

impl Rewind {
    /// Starts from the final document, which is `len` characters long.
    pub fn new(len: u32) -> Self {
        Rewind { pieces: PieceTable::new(len), len, text: Text::Ascii(vec![0; len as usize]) }
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
        let ascii = text.len() == len as usize;
        if let (false, Text::Ascii(bytes)) = (ascii, &self.text) {
            self.text = Text::Wide(bytes.iter().map(|&b| b as char).collect());
        }
        match &mut self.text {
            Text::Ascii(slots) => self.pieces.remove(pos, len, |at, skip, n| {
                let (at, skip, n) = (at as usize, skip as usize, n as usize);
                slots[at..at + n].copy_from_slice(&text.as_bytes()[skip..skip + n]);
            }),
            Text::Wide(slots) => {
                // The survivors come in the order they have in `text`, so one pass over it
                // reaches them all. `passed` counts the characters gone by.
                let (mut rest, mut passed) = (text.chars(), 0);
                self.pieces.remove(pos, len, |at, skip, n| {
                    let slots = &mut slots[at as usize..(at + n) as usize];
                    if ascii {
                        for (slot, &b) in slots.iter_mut().zip(&text.as_bytes()[skip as usize..]) {
                            *slot = b as char;
                        }
                    } else {
                        if skip > passed {
                            rest.nth((skip - passed - 1) as usize);
                        }
                        for (slot, ch) in slots.iter_mut().zip(rest.by_ref()) {
                            *slot = ch;
                        }
                        passed = skip + n;
                    }
                })
            }
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
        match self.text {
            Text::Ascii(bytes) => String::from_utf8(bytes).expect("ASCII"),
            Text::Wide(chars) => {
                // Mostly ASCII in practice: whole stretches of it are narrowed to bytes at once.
                let mut bytes = Vec::with_capacity(chars.len());
                for stretch in chars.chunks(16) {
                    if stretch.iter().all(char::is_ascii) {
                        bytes.extend(stretch.iter().map(|&ch| ch as u8));
                    } else {
                        stretch.iter().for_each(|&ch| bytes.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes()));
                    }
                }
                String::from_utf8(bytes).expect("encoded from characters")
            }
        }
    }
}
