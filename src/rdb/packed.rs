//! The three "packed container" formats RDB embeds inside a single string:
//! **listpack**, **ziplist**, and **intset**.
//!
//! meebis never writes these — every type it saves has a plain encoding that
//! real Redis still loads (see [`super::write`]) — but it must read them,
//! because Redis has no choice about emitting them. A 3-element list is a
//! `LIST_QUICKLIST_2` of listpack nodes; a small hash is a `HASH_LISTPACK`; a
//! set of small integers is a `SET_INTSET`. Ziplist is the pre-7.0 spelling of
//! listpack and shows up in dumps from Redis 5 and 6.
//!
//! All three walk untrusted bytes, so every accessor is bounds-checked and a
//! malformed container returns `None` rather than panicking.

use bytes::Bytes;

/// An element of a packed container. Both listpack and ziplist store small
/// integers in a dedicated encoding rather than as digits, so the decoders
/// preserve that distinction: callers wanting Redis' materialized form use
/// [`Element::into_bytes`], while the stream decoder needs the integers.
#[derive(Debug, Clone, PartialEq)]
pub enum Element {
    Int(i64),
    Str(Bytes),
}

impl Element {
    /// The value as Redis would materialize it — integers rendered as decimal.
    pub fn into_bytes(self) -> Bytes {
        match self {
            Element::Int(i) => Bytes::from(i.to_string()),
            Element::Str(s) => s,
        }
    }

    /// The integer value, if this element is stored as one.
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Element::Int(i) => Some(*i),
            Element::Str(_) => None,
        }
    }
}

fn le_u16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn le_u32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn le_u64(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// Sign-extend the low `bits` of `v`.
fn sign_extend(v: u64, bits: u32) -> i64 {
    let shift = 64 - bits;
    ((v << shift) as i64) >> shift
}

// ---------------------------------------------------------------- listpack

/// Number of bytes the trailing "backlen" field occupies for an entry whose
/// encoding+data span `n` bytes. Listpack stores it so the container can be
/// walked backwards; we only walk forwards, so we just need to step over it.
fn backlen_size(n: usize) -> usize {
    if n < 128 {
        1
    } else if n < 16384 {
        2
    } else if n < 2_097_152 {
        3
    } else if n < 268_435_456 {
        4
    } else {
        5
    }
}

/// Decode one listpack element at `pos`, returning it and its total footprint
/// (encoding + data + backlen). `None` at the `0xFF` terminator is signalled by
/// the caller checking for it first.
fn listpack_element(buf: &[u8], pos: usize) -> Option<(Element, usize)> {
    let b = *buf.get(pos)?;

    // Ordering matters: the 0xF0-0xF4 encodings must be matched before the
    // 12-bit-string mask, and the 7-bit and 6-bit masks before the 13-bit one.
    let (element, entry_len) = if b & 0x80 == 0 {
        // 7-bit unsigned integer, held entirely in the encoding byte.
        (Element::Int((b & 0x7f) as i64), 1)
    } else if b & 0xc0 == 0x80 {
        // 6-bit string length.
        let len = (b & 0x3f) as usize;
        let data = buf.get(pos + 1..pos + 1 + len)?;
        (Element::Str(Bytes::copy_from_slice(data)), 1 + len)
    } else if b & 0xe0 == 0xc0 {
        // 13-bit signed integer.
        let raw = (((b & 0x1f) as u64) << 8) | *buf.get(pos + 1)? as u64;
        (Element::Int(sign_extend(raw, 13)), 2)
    } else if b == 0xf1 {
        (Element::Int(le_u16(buf, pos + 1)? as i16 as i64), 3)
    } else if b == 0xf2 {
        let raw = (*buf.get(pos + 1)? as u64)
            | ((*buf.get(pos + 2)? as u64) << 8)
            | ((*buf.get(pos + 3)? as u64) << 16);
        (Element::Int(sign_extend(raw, 24)), 4)
    } else if b == 0xf3 {
        (Element::Int(le_u32(buf, pos + 1)? as i32 as i64), 5)
    } else if b == 0xf4 {
        (Element::Int(le_u64(buf, pos + 1)? as i64), 9)
    } else if b == 0xf0 {
        // 32-bit string length (little-endian, unlike ziplist's big-endian).
        let len = le_u32(buf, pos + 1)? as usize;
        let data = buf.get(pos + 5..pos + 5 + len)?;
        (Element::Str(Bytes::copy_from_slice(data)), 5 + len)
    } else if b & 0xf0 == 0xe0 {
        // 12-bit string length.
        let len = (((b & 0x0f) as usize) << 8) | *buf.get(pos + 1)? as usize;
        let data = buf.get(pos + 2..pos + 2 + len)?;
        (Element::Str(Bytes::copy_from_slice(data)), 2 + len)
    } else {
        return None; // 0xF5..0xFE are not valid encodings.
    };

    Some((element, entry_len + backlen_size(entry_len)))
}

/// Decode a whole listpack: `<total-bytes u32><num-elements u16><elements><0xFF>`.
///
/// The header's element count is only a hint (it saturates at `u16::MAX`), so
/// the walk is terminated by the `0xFF` byte, not by counting.
pub fn listpack(buf: &[u8]) -> Option<Vec<Element>> {
    if buf.len() < 7 {
        return None;
    }
    let mut out = Vec::new();
    let mut pos = 6; // skip total-bytes + num-elements
    loop {
        match buf.get(pos)? {
            0xff => return Some(out),
            _ => {
                let (element, size) = listpack_element(buf, pos)?;
                out.push(element);
                pos += size;
            }
        }
    }
}

/// A listpack decoded to Redis' materialized form.
pub fn listpack_strings(buf: &[u8]) -> Option<Vec<Bytes>> {
    Some(
        listpack(buf)?
            .into_iter()
            .map(Element::into_bytes)
            .collect(),
    )
}

// ----------------------------------------------------------------- ziplist

/// Decode one ziplist entry at `pos`, returning it and its total footprint.
fn ziplist_element(buf: &[u8], pos: usize) -> Option<(Element, usize)> {
    // Every entry opens with the previous entry's length, which we skip.
    let prevlen_size = if *buf.get(pos)? < 254 { 1 } else { 5 };
    let p = pos + prevlen_size;
    let b = *buf.get(p)?;

    let (element, rest) = match b >> 6 {
        0 => {
            // 6-bit string length.
            let len = (b & 0x3f) as usize;
            let data = buf.get(p + 1..p + 1 + len)?;
            (Element::Str(Bytes::copy_from_slice(data)), 1 + len)
        }
        1 => {
            // 14-bit string length, big-endian.
            let len = (((b & 0x3f) as usize) << 8) | *buf.get(p + 1)? as usize;
            let data = buf.get(p + 2..p + 2 + len)?;
            (Element::Str(Bytes::copy_from_slice(data)), 2 + len)
        }
        2 => {
            // 32-bit string length, big-endian.
            let raw = buf.get(p + 1..p + 5)?;
            let len = u32::from_be_bytes(raw.try_into().ok()?) as usize;
            let data = buf.get(p + 5..p + 5 + len)?;
            (Element::Str(Bytes::copy_from_slice(data)), 5 + len)
        }
        _ => match b {
            0xc0 => (Element::Int(le_u16(buf, p + 1)? as i16 as i64), 3),
            0xd0 => (Element::Int(le_u32(buf, p + 1)? as i32 as i64), 5),
            0xe0 => (Element::Int(le_u64(buf, p + 1)? as i64), 9),
            0xf0 => {
                let raw = (*buf.get(p + 1)? as u64)
                    | ((*buf.get(p + 2)? as u64) << 8)
                    | ((*buf.get(p + 3)? as u64) << 16);
                (Element::Int(sign_extend(raw, 24)), 4)
            }
            0xfe => (Element::Int(*buf.get(p + 1)? as i8 as i64), 2),
            // 0xF1..=0xFD hold a 4-bit value inline, biased by one.
            0xf1..=0xfd => (Element::Int(((b & 0x0f) as i64) - 1), 1),
            _ => return None,
        },
    };

    Some((element, prevlen_size + rest))
}

/// Decode a whole ziplist: `<zlbytes u32><zltail u32><zllen u16><entries><0xFF>`.
pub fn ziplist(buf: &[u8]) -> Option<Vec<Element>> {
    if buf.len() < 11 {
        return None;
    }
    let mut out = Vec::new();
    let mut pos = 10; // skip zlbytes + zltail + zllen
    loop {
        match buf.get(pos)? {
            0xff => return Some(out),
            _ => {
                let (element, size) = ziplist_element(buf, pos)?;
                out.push(element);
                pos += size;
            }
        }
    }
}

/// A ziplist decoded to Redis' materialized form.
pub fn ziplist_strings(buf: &[u8]) -> Option<Vec<Bytes>> {
    Some(ziplist(buf)?.into_iter().map(Element::into_bytes).collect())
}

// ------------------------------------------------------------------ intset

/// Decode an intset: `<encoding u32><length u32><values>`, where `encoding` is
/// the width in bytes of each little-endian signed value.
pub fn intset(buf: &[u8]) -> Option<Vec<Bytes>> {
    let width = le_u32(buf, 0)? as usize;
    let count = le_u32(buf, 4)? as usize;
    if !matches!(width, 2 | 4 | 8) {
        return None;
    }
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let at = 8 + i * width;
        let v = match width {
            2 => le_u16(buf, at)? as i16 as i64,
            4 => le_u32(buf, at)? as i32 as i64,
            _ => le_u64(buf, at)? as i64,
        };
        out.push(Bytes::from(v.to_string()));
    }
    Some(out)
}

// --------------------------------------------------------- listpack writing

/// Builds a listpack. The only encoder here: meebis writes plain forms for
/// every type except streams, which have no plain form (see [`super::stream`]).
///
/// Each element picks the narrowest encoding that fits, because Redis
/// revalidates the container on load and a needlessly wide encoding is more
/// bytes for no benefit.
pub struct ListpackWriter {
    body: Vec<u8>,
    elements: usize,
}

impl ListpackWriter {
    pub fn new() -> ListpackWriter {
        ListpackWriter {
            body: Vec::new(),
            elements: 0,
        }
    }

    /// Append the variable-length "backlen" trailer describing an entry of
    /// `len` bytes. Stored most-significant byte first, with the continuation
    /// bit set on every byte but the first, so it can be read backwards.
    fn backlen(&mut self, len: usize) {
        let mut stack = [0u8; 5];
        let mut n = 0;
        if len <= 127 {
            self.body.push(len as u8);
            return;
        }
        // Emit the low 7 bits first into a scratch buffer, then reverse.
        let mut remaining = len;
        while remaining > 0 {
            stack[n] = (remaining & 127) as u8;
            remaining >>= 7;
            n += 1;
        }
        for i in (0..n).rev() {
            // Every byte after the first carries the continuation marker.
            let marker = if i == n - 1 { 0 } else { 128 };
            self.body.push(stack[i] | marker);
        }
    }

    pub fn int(&mut self, v: i64) {
        let start = self.body.len();
        if (0..=127).contains(&v) {
            self.body.push(v as u8);
        } else if (-4096..=4095).contains(&v) {
            let raw = (v as u64) & 0x1fff;
            self.body.push(0xc0 | ((raw >> 8) as u8));
            self.body.push((raw & 0xff) as u8);
        } else if (i16::MIN as i64..=i16::MAX as i64).contains(&v) {
            self.body.push(0xf1);
            self.body.extend_from_slice(&(v as i16).to_le_bytes());
        } else if (-(1 << 23)..(1 << 23)).contains(&v) {
            self.body.push(0xf2);
            self.body.extend_from_slice(&(v as i32).to_le_bytes()[..3]);
        } else if (i32::MIN as i64..=i32::MAX as i64).contains(&v) {
            self.body.push(0xf3);
            self.body.extend_from_slice(&(v as i32).to_le_bytes());
        } else {
            self.body.push(0xf4);
            self.body.extend_from_slice(&v.to_le_bytes());
        }
        let len = self.body.len() - start;
        self.backlen(len);
        self.elements += 1;
    }

    pub fn str(&mut self, s: &[u8]) {
        let start = self.body.len();
        if s.len() < 64 {
            self.body.push(0x80 | s.len() as u8);
        } else if s.len() < 4096 {
            self.body.push(0xe0 | ((s.len() >> 8) as u8));
            self.body.push((s.len() & 0xff) as u8);
        } else {
            self.body.push(0xf0);
            self.body.extend_from_slice(&(s.len() as u32).to_le_bytes());
        }
        self.body.extend_from_slice(s);
        let len = self.body.len() - start;
        self.backlen(len);
        self.elements += 1;
    }

    /// Close the listpack: prepend the header and append the terminator.
    pub fn finish(self) -> Vec<u8> {
        let total = 6 + self.body.len() + 1;
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(&(total as u32).to_le_bytes());
        // 65535 is the "unknown, go count them" sentinel for large listpacks.
        out.extend_from_slice(&(self.elements.min(65535) as u16).to_le_bytes());
        out.extend_from_slice(&self.body);
        out.push(0xff);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a listpack around pre-encoded element bytes. The header's element
    /// count is deliberately wrong to prove the walk is terminator-driven.
    fn wrap_listpack(elements: &[u8]) -> Vec<u8> {
        let total = 6 + elements.len() + 1;
        let mut out = Vec::new();
        out.extend_from_slice(&(total as u32).to_le_bytes());
        out.extend_from_slice(&999u16.to_le_bytes());
        out.extend_from_slice(elements);
        out.push(0xff);
        out
    }

    #[test]
    fn listpack_7bit_and_6bit_encodings() {
        // 5 (7-bit uint, backlen 1) then "hi" (6-bit str, backlen 1).
        let lp = wrap_listpack(&[0x05, 1, 0x82, b'h', b'i', 3]);
        assert_eq!(
            listpack(&lp).unwrap(),
            vec![Element::Int(5), Element::Str(Bytes::from("hi"))]
        );
    }

    #[test]
    fn listpack_13bit_int_is_signed() {
        // 0xC0|0x1F, 0xFF encodes -1 across the full 13-bit field.
        let lp = wrap_listpack(&[0xdf, 0xff, 2]);
        assert_eq!(listpack(&lp).unwrap(), vec![Element::Int(-1)]);
    }

    #[test]
    fn listpack_wide_int_encodings() {
        let mut e = Vec::new();
        e.push(0xf1);
        e.extend_from_slice(&(-300i16).to_le_bytes());
        e.push(3);
        e.push(0xf4);
        e.extend_from_slice(&(-1_234_567_890_123i64).to_le_bytes());
        e.push(9);
        assert_eq!(
            listpack(&wrap_listpack(&e)).unwrap(),
            vec![Element::Int(-300), Element::Int(-1_234_567_890_123)]
        );
    }

    #[test]
    fn listpack_24bit_int_sign_extends() {
        let mut e = vec![0xf2];
        e.extend_from_slice(&[0xff, 0xff, 0xff]); // -1 in 24 bits
        e.push(4);
        assert_eq!(
            listpack(&wrap_listpack(&e)).unwrap(),
            vec![Element::Int(-1)]
        );
    }

    #[test]
    fn listpack_12bit_string() {
        let body = vec![b'x'; 300];
        let mut e = vec![0xe0 | ((300 >> 8) as u8), (300 & 0xff) as u8];
        e.extend_from_slice(&body);
        // A 302-byte entry needs a two-byte backlen: high 7 bits first, then
        // the low 7 with the continuation bit set.
        e.push((302 >> 7) as u8);
        e.push(((302 & 127) | 128) as u8);
        let decoded = listpack(&wrap_listpack(&e)).unwrap();
        assert_eq!(decoded, vec![Element::Str(Bytes::from(body))]);
    }

    #[test]
    fn listpack_rejects_truncation() {
        // Claims a 6-bit string of 10 bytes but supplies none.
        assert!(listpack(&wrap_listpack(&[0x8a])).is_none());
        assert!(listpack(&[1, 2, 3]).is_none());
    }

    #[test]
    fn ints_render_as_decimal_strings() {
        let lp = wrap_listpack(&[0x05, 1]);
        assert_eq!(listpack_strings(&lp).unwrap(), vec![Bytes::from("5")]);
    }

    fn wrap_ziplist(entries: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&0u32.to_le_bytes()); // zlbytes
        out.extend_from_slice(&0u32.to_le_bytes()); // zltail
        out.extend_from_slice(&0u16.to_le_bytes()); // zllen
        out.extend_from_slice(entries);
        out.push(0xff);
        out
    }

    #[test]
    fn ziplist_string_and_immediate_int() {
        // prevlen 0, 6-bit str "ab"; then prevlen 3, immediate int 4 (0xF5).
        let zl = wrap_ziplist(&[0x00, 0x02, b'a', b'b', 0x03, 0xf5]);
        assert_eq!(
            ziplist(&zl).unwrap(),
            vec![Element::Str(Bytes::from("ab")), Element::Int(4)]
        );
    }

    #[test]
    fn ziplist_14bit_length_is_big_endian() {
        let body = vec![b'z'; 200];
        let mut e = vec![0x00, 0x40 | ((200 >> 8) as u8), (200 & 0xff) as u8];
        e.extend_from_slice(&body);
        assert_eq!(
            ziplist(&wrap_ziplist(&e)).unwrap(),
            vec![Element::Str(Bytes::from(body))]
        );
    }

    #[test]
    fn ziplist_five_byte_prevlen_is_skipped() {
        let mut e = vec![0xfe, 0, 0, 0, 0]; // large prevlen marker + 4 bytes
        e.extend_from_slice(&[0x01, b'q']);
        assert_eq!(
            ziplist(&wrap_ziplist(&e)).unwrap(),
            vec![Element::Str(Bytes::from("q"))]
        );
    }

    #[test]
    fn intset_reads_each_width() {
        let mut b = Vec::new();
        b.extend_from_slice(&2u32.to_le_bytes());
        b.extend_from_slice(&3u32.to_le_bytes());
        for v in [-5i16, 0, 300] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        assert_eq!(
            intset(&b).unwrap(),
            vec![Bytes::from("-5"), Bytes::from("0"), Bytes::from("300")]
        );
    }

    #[test]
    fn intset_rejects_bad_width_and_truncation() {
        let mut b = Vec::new();
        b.extend_from_slice(&3u32.to_le_bytes()); // 3 is not a legal width
        b.extend_from_slice(&1u32.to_le_bytes());
        assert!(intset(&b).is_none());

        let mut c = Vec::new();
        c.extend_from_slice(&8u32.to_le_bytes());
        c.extend_from_slice(&5u32.to_le_bytes()); // claims 5 values, supplies 0
        assert!(intset(&c).is_none());
    }

    /// The writer and the decoder are the two halves that have to agree on
    /// every encoding boundary, so drive values that sit exactly on them.
    #[test]
    fn listpack_writer_round_trips_every_encoding() {
        let ints = [
            0,
            127,
            128,
            -1,
            4095,
            -4096,
            4096,
            -4097,
            32767,
            -32768,
            32768,
            8_388_607,
            -8_388_608,
            8_388_608,
            2_147_483_647,
            -2_147_483_648,
            2_147_483_648,
            i64::MAX,
            i64::MIN,
        ];
        let strings: Vec<Vec<u8>> = vec![
            b"".to_vec(),
            b"short".to_vec(),
            vec![b'a'; 63],
            vec![b'b'; 64],
            vec![b'c'; 4095],
            vec![b'd'; 4096],
            vec![b'e'; 70_000],
        ];

        let mut w = ListpackWriter::new();
        for i in ints {
            w.int(i);
        }
        for s in &strings {
            w.str(s);
        }
        let encoded = w.finish();

        let decoded = listpack(&encoded).expect("writer produced an undecodable listpack");
        let mut expected: Vec<Element> = ints.iter().map(|i| Element::Int(*i)).collect();
        expected.extend(strings.iter().map(|s| Element::Str(Bytes::from(s.clone()))));
        assert_eq!(decoded, expected);
    }

    /// The header's total-bytes field is what Redis validates first on load;
    /// if it disagrees with the real length the file is rejected outright.
    #[test]
    fn listpack_writer_header_matches_actual_length() {
        let mut w = ListpackWriter::new();
        w.str(b"hello");
        w.int(42);
        let encoded = w.finish();
        let claimed = u32::from_le_bytes(encoded[..4].try_into().unwrap()) as usize;
        assert_eq!(claimed, encoded.len());
        assert_eq!(u16::from_le_bytes(encoded[4..6].try_into().unwrap()), 2);
        assert_eq!(*encoded.last().unwrap(), 0xff);
    }

    /// Backlen is the one field a forward walk computes rather than reads, so
    /// a mismatch would desync the decoder at the first multi-byte entry.
    #[test]
    fn listpack_backlen_width_agrees_with_the_decoder() {
        for len in [1usize, 127, 128, 200, 16383, 16384, 20000] {
            let mut w = ListpackWriter::new();
            w.str(&vec![b'x'; len]);
            w.int(7); // a follower that only decodes if the backlen was sized right
            let decoded = listpack(&w.finish()).unwrap();
            assert_eq!(decoded.len(), 2, "desynced at string length {len}");
            assert_eq!(decoded[1], Element::Int(7));
        }
    }
}
