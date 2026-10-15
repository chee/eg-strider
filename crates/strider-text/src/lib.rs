mod pieces;

use pieces::PieceList;
use std::ops::Range;

/// Builds a document's final text by taking its edits back, last to first, over a piece table
/// of the final text. Taking back an insert copies its surviving characters straight to their
/// final place, so no rope is needed. Positions and lengths count characters.
pub struct PieceTable<'a> {
    /// The text of every insert.
    inserted_text: &'a str,
    pieces: PieceList,
    /// Length of the document as taken back so far.
    len: u32,
    /// The final text as it is placed: a byte per character, then [`SHORT`] spare bytes. A
    /// character that is not ASCII is [`NON_ASCII_MARK`] here and kept in `non_ascii`.
    final_text_bytes: Vec<u8>,
    non_ascii: NonAsciiChars,
}

/// Stands in for a character that does not fit in a byte.
const NON_ASCII_MARK: u8 = 0x80;

/// The longest copy done as two 16-byte words.
const SHORT: usize = 32;

/// Copies `n` bytes of `from`, from `skip`, to `to[at..]`, which must have [`SHORT`] bytes to
/// spare.
fn copy(to: &mut [u8], at: usize, from: &[u8], skip: usize, n: usize) {
    if n > SHORT || skip + SHORT > from.len() {
        to[at..at + n].copy_from_slice(&from[skip..skip + n]);
        return;
    }
    // Most copies are a few bytes long and no two alike, and a general copy branches on the
    // length in ways that cannot be predicted. These take the same steps whatever their
    // length: two words are read from each side, and the bytes past the end are put back
    // as they were.
    let word = |bytes: &[u8]| u128::from_le_bytes(bytes[..16].try_into().expect("16 bytes"));
    for (start, n) in [(0, n.min(16)), (16, n.saturating_sub(16))] {
        let beyond = u128::MAX.checked_shl(8 * n as u32).unwrap_or(0);
        let merged = word(&to[at + start..]) & beyond | word(&from[skip + start..]) & !beyond;
        to[at + start..at + start + 16].copy_from_slice(&merged.to_le_bytes());
    }
}

/// Positions per page of [`NonAsciiChars`]. Tiny in debug builds, so that tests use several
/// pages.
const NON_ASCII_PAGE: usize = if cfg!(debug_assertions) { 4 } else { 256 };

/// The final text's characters that are not ASCII, by position, with room only for pages that
/// have some.
#[derive(Default)]
struct NonAsciiChars {
    /// Per page, where its characters start in `chars`, if it has any.
    pages: Vec<Option<u32>>,
    chars: Vec<char>,
}

impl NonAsciiChars {
    /// Records the character at position `at`.
    #[cold]
    fn set(&mut self, at: usize, ch: char) {
        if self.pages.len() <= at / NON_ASCII_PAGE {
            self.pages.resize(at / NON_ASCII_PAGE + 1, None);
        }
        let start = *self.pages[at / NON_ASCII_PAGE].get_or_insert_with(|| {
            self.chars.resize(self.chars.len() + NON_ASCII_PAGE, '\0');
            (self.chars.len() - NON_ASCII_PAGE) as u32
        });
        self.chars[start as usize + at % NON_ASCII_PAGE] = ch;
    }

    /// Returns the character recorded at position `at`.
    fn get(&self, at: usize) -> char {
        let start = self.pages[at / NON_ASCII_PAGE].expect("a character was recorded on this page");
        self.chars[start as usize + at % NON_ASCII_PAGE]
    }
}

impl<'a> PieceTable<'a> {
    /// Starts from the final document, `len` characters long. `inserted_text` holds the text of
    /// every insert.
    pub fn new(len: u32, inserted_text: &'a str) -> Self {
        PieceTable { inserted_text, pieces: PieceList::new(len), len, final_text_bytes: vec![0; len as usize + SHORT], non_ascii: NonAsciiChars::default() }
    }

    /// Takes back an insert of the `len` characters at `text` in `inserted_text`, at `pos`.
    pub fn undo_insert(&mut self, pos: u32, text: Range<usize>, len: u32) {
        assert!(pos.checked_add(len).is_some_and(|end| end <= self.len), "insert was not within the document");
        let (inserted_text, start) = (self.inserted_text.as_bytes(), text.start);
        let text = &self.inserted_text[text];
        // Debug builds only: counting takes as long as the text.
        debug_assert_eq!(text.chars().count(), len as usize);
        if len == 0 {
            return;
        }
        self.len -= len;
        let (final_text_bytes, non_ascii) = (&mut self.final_text_bytes, &mut self.non_ascii);
        if text.len() == len as usize {
            // All ASCII: characters and bytes coincide.
            self.pieces.remove(pos, len, |at, skip, n| copy(final_text_bytes, at as usize, inserted_text, start + skip as usize, n as usize));
        } else {
            // The survivors come in the order they have in `text`, so one pass over it
            // reaches them all. `passed` counts the characters gone by.
            let (mut rest, mut passed) = (text.chars(), 0);
            self.pieces.remove(pos, len, |at, skip, n| {
                if skip > passed {
                    rest.nth((skip - passed - 1) as usize);
                }
                for (at, ch) in (at as usize..).zip(rest.by_ref().take(n as usize)) {
                    final_text_bytes[at] = if ch.is_ascii() { ch as u8 } else { NON_ASCII_MARK };
                    if !ch.is_ascii() {
                        non_ascii.set(at, ch);
                    }
                }
                passed = skip + n;
            });
        }
    }

    /// Takes back a removal of `len` characters at `pos`.
    pub fn undo_remove(&mut self, pos: u32, len: u32) {
        assert!(pos <= self.len, "removal was not within the document");
        if len == 0 {
            return;
        }
        self.len = self.len.checked_add(len).expect("document too long");
        self.pieces.insert_deleted(pos, len);
    }

    /// Takes back all remaining edits at once, given the text they made. What survives of
    /// `text` goes straight to its final place.
    pub fn undo_rest(&mut self, text: &str) {
        let (final_text_bytes, non_ascii) = (&mut self.final_text_bytes, &mut self.non_ascii);
        if text.len() == self.len as usize {
            // All ASCII: characters and bytes coincide.
            let mut from = 0;
            for (len, at) in self.pieces.iter() {
                if let Some(at) = at {
                    final_text_bytes[at as usize..(at + len) as usize].copy_from_slice(&text.as_bytes()[from..from + len as usize]);
                }
                from += len as usize;
            }
        } else {
            let mut rest = text.chars();
            for (len, at) in self.pieces.iter() {
                let Some(at) = at else {
                    rest.nth(len as usize - 1);
                    continue;
                };
                for (at, ch) in (at as usize..).zip(rest.by_ref().take(len as usize)) {
                    final_text_bytes[at] = if ch.is_ascii() { ch as u8 } else { NON_ASCII_MARK };
                    if !ch.is_ascii() {
                        non_ascii.set(at, ch);
                    }
                }
            }
            assert!(rest.next().is_none(), "the text is not the document as it stands");
        }
        self.len = 0;
    }

    /// Returns the final text. Every edit must be taken back.
    pub fn into_string(mut self) -> String {
        assert_eq!(self.len, 0, "edits left to take back");
        self.final_text_bytes.truncate(self.final_text_bytes.len() - SHORT);
        if self.non_ascii.chars.is_empty() {
            return String::from_utf8(self.final_text_bytes).expect("ASCII");
        }
        // Mostly ASCII in practice: whole stretches of it are copied at once.
        let mut bytes = Vec::with_capacity(self.final_text_bytes.len() + self.non_ascii.chars.len());
        for (i, stretch) in self.final_text_bytes.chunks(16).enumerate() {
            if stretch.is_ascii() {
                bytes.extend_from_slice(stretch);
            } else {
                for (at, &byte) in (i * 16..).zip(stretch) {
                    let ch = if byte == NON_ASCII_MARK { self.non_ascii.get(at) } else { byte as char };
                    bytes.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes());
                }
            }
        }
        String::from_utf8(bytes).expect("encoded from characters")
    }
}
