//! A bounds-checked cursor over an RDB image, plus the two primitive codecs
//! every value type is built from: the length encoding and the string encoding.
//!
//! Shared by [`super::read`] and [`super::stream`]. Every accessor returns a
//! [`Error::Corrupt`] carrying the byte offset rather than panicking — this
//! parses a file meebis did not write, and a truncated dump must not take the
//! server down with it.

use super::{Error, Result};
use bytes::Bytes;

/// A length field, which in RDB doubles as a string-encoding discriminator.
pub enum Len {
    /// A genuine count.
    Plain(u64),
    /// One of the special string encodings (int8/16/32 or LZF).
    Special(u8),
}

pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Reader<'a> {
        Reader { buf, pos: 0 }
    }

    pub fn seek(&mut self, pos: usize) {
        self.pos = pos;
    }

    pub fn corrupt(&self, what: &str) -> Error {
        Error::Corrupt(format!("{what} at byte {}", self.pos))
    }

    pub fn byte(&mut self) -> Result<u8> {
        let b = *self
            .buf
            .get(self.pos)
            .ok_or_else(|| self.corrupt("unexpected end of file"))?;
        self.pos += 1;
        Ok(b)
    }

    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| self.corrupt("length overflow"))?;
        let slice = self
            .buf
            .get(self.pos..end)
            .ok_or_else(|| self.corrupt("unexpected end of file"))?;
        self.pos = end;
        Ok(slice)
    }

    pub fn u32le(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn u64le(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    /// The RDB length encoding: two leading bits select a 6-bit, 14-bit,
    /// 32-bit, or 64-bit count, or mark the value as a special string encoding.
    pub fn length(&mut self) -> Result<Len> {
        let b = self.byte()?;
        match b >> 6 {
            0 => Ok(Len::Plain((b & 0x3f) as u64)),
            1 => {
                let lo = self.byte()?;
                Ok(Len::Plain((((b & 0x3f) as u64) << 8) | lo as u64))
            }
            3 => Ok(Len::Special(b & 0x3f)),
            _ => match b {
                0x80 => Ok(Len::Plain(
                    u32::from_be_bytes(self.take(4)?.try_into().unwrap()) as u64,
                )),
                0x81 => Ok(Len::Plain(u64::from_be_bytes(
                    self.take(8)?.try_into().unwrap(),
                ))),
                _ => Err(self.corrupt("unknown length encoding")),
            },
        }
    }

    /// A plain count, rejecting the special string encodings.
    pub fn count(&mut self) -> Result<usize> {
        match self.length()? {
            Len::Plain(n) => usize::try_from(n).map_err(|_| self.corrupt("length too large")),
            Len::Special(_) => Err(self.corrupt("expected a length, found a string encoding")),
        }
    }

    /// A count used as a 64-bit quantity (stream ids and counters).
    pub fn count_u64(&mut self) -> Result<u64> {
        match self.length()? {
            Len::Plain(n) => Ok(n),
            Len::Special(_) => Err(self.corrupt("expected a length, found a string encoding")),
        }
    }

    /// A string: raw bytes, an integer rendered as decimal, or an LZF block.
    pub fn string(&mut self) -> Result<Bytes> {
        match self.length()? {
            Len::Plain(n) => {
                let n = usize::try_from(n).map_err(|_| self.corrupt("string too large"))?;
                Ok(Bytes::copy_from_slice(self.take(n)?))
            }
            Len::Special(0) => Ok(Bytes::from((self.byte()? as i8).to_string())),
            Len::Special(1) => {
                let v = i16::from_le_bytes(self.take(2)?.try_into().unwrap());
                Ok(Bytes::from(v.to_string()))
            }
            Len::Special(2) => {
                let v = i32::from_le_bytes(self.take(4)?.try_into().unwrap());
                Ok(Bytes::from(v.to_string()))
            }
            Len::Special(3) => {
                let compressed = self.count()?;
                let expanded = self.count()?;
                let block = self.take(compressed)?;
                super::lzf::decompress(block, expanded)
                    .map(Bytes::from)
                    .ok_or_else(|| self.corrupt("malformed LZF block"))
            }
            Len::Special(other) => Err(Error::Corrupt(format!(
                "unknown string encoding {other} at byte {}",
                self.pos
            ))),
        }
    }

    /// `ZSET_2` scores: a raw little-endian double.
    pub fn binary_double(&mut self) -> Result<f64> {
        Ok(f64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    /// Pre-3.0 `ZSET` scores: a length-prefixed ASCII rendering, with three
    /// reserved lengths standing in for the non-finite values.
    pub fn string_double(&mut self) -> Result<f64> {
        match self.byte()? {
            255 => Ok(f64::NEG_INFINITY),
            254 => Ok(f64::INFINITY),
            253 => Ok(f64::NAN),
            n => {
                let raw = self.take(n as usize)?;
                std::str::from_utf8(raw)
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| self.corrupt("unparseable score"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn six_and_fourteen_bit_lengths() {
        let mut r = Reader::new(&[0x0a, 0x40 | 0x01, 0x2c]);
        assert!(matches!(r.length(), Ok(Len::Plain(10))));
        assert!(matches!(r.length(), Ok(Len::Plain(300))));
    }

    #[test]
    fn wide_lengths_are_big_endian() {
        let mut buf = vec![0x80];
        buf.extend_from_slice(&70000u32.to_be_bytes());
        buf.push(0x81);
        buf.extend_from_slice(&5_000_000_000u64.to_be_bytes());
        let mut r = Reader::new(&buf);
        assert!(matches!(r.length(), Ok(Len::Plain(70000))));
        assert!(matches!(r.length(), Ok(Len::Plain(5_000_000_000))));
    }

    #[test]
    fn integer_encoded_strings_become_decimal() {
        // int8 -5, int16 -300, int32 100000
        let mut buf = vec![0xc0, (-5i8) as u8, 0xc1];
        buf.extend_from_slice(&(-300i16).to_le_bytes());
        buf.push(0xc2);
        buf.extend_from_slice(&100000i32.to_le_bytes());
        let mut r = Reader::new(&buf);
        assert_eq!(r.string().unwrap(), Bytes::from("-5"));
        assert_eq!(r.string().unwrap(), Bytes::from("-300"));
        assert_eq!(r.string().unwrap(), Bytes::from("100000"));
    }

    #[test]
    fn truncation_is_an_error_not_a_panic() {
        let mut r = Reader::new(&[0x05, b'a']);
        assert!(r.string().is_err());
        let mut r = Reader::new(&[]);
        assert!(r.byte().is_err());
    }

    #[test]
    fn non_finite_string_doubles() {
        let mut r = Reader::new(&[255, 254, 253]);
        assert_eq!(r.string_double().unwrap(), f64::NEG_INFINITY);
        assert_eq!(r.string_double().unwrap(), f64::INFINITY);
        assert!(r.string_double().unwrap().is_nan());
    }
}
