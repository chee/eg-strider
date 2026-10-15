use super::file::decompress_checked;
use crate::malformed::Malformed;

/// What is left to read of a file or a column.
pub(super) struct Reader<'a>(pub(super) &'a [u8]);

impl<'a> Reader<'a> {
    #[inline]
    pub(super) fn varint(&mut self) -> Result<u64, Malformed> {
        // Most numbers fit in a byte.
        if let Some((&byte, rest)) = self.0.split_first().filter(|(&byte, _)| byte < 0x80) {
            self.0 = rest;
            return Ok(u64::from(byte));
        }
        let mut n = 0;
        for shift in (0..64).step_by(7) {
            let (&byte, rest) = self.0.split_first().ok_or(Malformed)?;
            self.0 = rest;
            n |= u64::from(byte & 0x7f) << shift;
            if byte < 0x80 {
                return Ok(n);
            }
        }
        Err(Malformed)
    }

    #[inline]
    pub(super) fn number(&mut self) -> Result<u32, Malformed> {
        u32::try_from(self.varint()?).map_err(|_| Malformed)
    }

    /// Reads a count of things at least a byte each.
    pub(super) fn count(&mut self) -> Result<usize, Malformed> {
        let items = self.number()? as usize;
        (items <= self.0.len()).then_some(items).ok_or(Malformed)
    }

    /// Takes the next `len` bytes, which [`count`](Self::count) has checked are there.
    pub(super) fn take(&mut self, len: usize) -> &'a [u8] {
        let (bytes, rest) = self.0.split_at(len);
        self.0 = rest;
        bytes
    }

    /// Reads a column that [`column`](fn@super::file::column) wrote.
    pub(super) fn column(&mut self) -> Result<Reader<'a>, Malformed> {
        let len = self.count()?;
        Ok(Reader(self.take(len)))
    }

    /// Reads a compressed section: its length, its compressed length, then the bytes.
    pub(super) fn compressed(&mut self) -> Result<Vec<u8>, Malformed> {
        let (len, compressed_len) = (self.number()? as usize, self.count()?);
        decompress_checked(self.take(compressed_len), len)
    }
}

/// Reads values that [`runs`](super::file::runs) wrote.
pub(super) struct Runs<'a> {
    from: Reader<'a>,
    value: u64,
    left: u64,
}

impl<'a> Runs<'a> {
    pub(super) fn of(from: Reader<'a>) -> Self {
        Runs { from, value: 0, left: 0 }
    }

    #[inline]
    pub(super) fn next(&mut self) -> Result<u64, Malformed> {
        if self.left == 0 {
            (self.value, self.left) = (self.from.varint()?, self.from.varint()?);
            if self.left == 0 {
                return Err(Malformed);
            }
        }
        self.left -= 1;
        Ok(self.value)
    }
}
