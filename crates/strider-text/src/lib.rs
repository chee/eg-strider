//! Works out a document's final text from the edits that were made to it.

mod runs;

use runs::Runs;

/// A document being taken back from its final state to empty, one edit at a time, last edit
/// first. Positions and lengths count characters.
pub struct Rewind {
    runs: Runs,
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
        Rewind { runs: Runs::new(len), len, text: Text::Ascii(vec![0; len as usize]) }
    }

    /// Takes back an insert that put `text`, `len` characters long, at `pos`.
    pub fn undo_insert(&mut self, pos: u32, text: &str, len: u32) {
        assert!(pos.checked_add(len).is_some_and(|end| end <= self.len), "insert was not within the document");
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
            Text::Ascii(slots) => self.runs.remove(pos, len, |at, skip, n| {
                let (at, skip, n) = (at as usize, skip as usize, n as usize);
                slots[at..at + n].copy_from_slice(&text.as_bytes()[skip..skip + n]);
            }),
            Text::Wide(slots) => {
                // The survivors come in the order they have in `text`, so one pass over it
                // reaches them all. `passed` counts the characters gone by.
                let (mut rest, mut passed) = (text.chars(), 0);
                self.runs.remove(pos, len, |at, skip, n| {
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
        self.runs.insert_doomed(pos, len);
    }

    /// The final text.
    pub fn finish(self) -> String {
        assert_eq!(self.len, 0, "edits left to take back");
        match self.text {
            Text::Ascii(bytes) => String::from_utf8(bytes).expect("ASCII"),
            Text::Wide(chars) => chars.into_iter().collect(),
        }
    }
}
