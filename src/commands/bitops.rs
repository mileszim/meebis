//! Bitmap commands, operating on the raw bytes of string values.
//!
//! Redis numbers bits most-significant-first within each byte: bit offset 0 is
//! the top bit of byte 0.

use super::{parse_int, upper, wrong_args};
use crate::db::{Db, Value};
use crate::resp::Frame;
use bytes::Bytes;

/// Redis caps bit offsets at 2^32 (512 MB of string).
const MAX_BIT_OFFSET: i64 = 4 * 1024 * 1024 * 1024;

fn get_bit(bytes: &[u8], bit: usize) -> u8 {
    let byte = bit / 8;
    if byte >= bytes.len() {
        return 0;
    }
    (bytes[byte] >> (7 - (bit % 8))) & 1
}

pub fn setbit(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() != 4 {
        return wrong_args("setbit");
    }
    let offset = match parse_int(&args[2]) {
        Ok(n) if n >= 0 && n < MAX_BIT_OFFSET => n as usize,
        _ => return Frame::err("bit offset is not an integer or out of range"),
    };
    let value = match parse_int(&args[3]) {
        Ok(0) => 0u8,
        Ok(1) => 1u8,
        _ => return Frame::err("bit is not an integer or out of range"),
    };
    let mut buf = match db.get_str(&args[1]) {
        Ok(Some(s)) => s.to_vec(),
        Ok(None) => Vec::new(),
        Err(_) => return Frame::wrongtype(),
    };
    let byte = offset / 8;
    if buf.len() <= byte {
        buf.resize(byte + 1, 0);
    }
    let mask = 1u8 << (7 - (offset % 8));
    let old = (buf[byte] & mask != 0) as i64;
    if value == 1 {
        buf[byte] |= mask;
    } else {
        buf[byte] &= !mask;
    }
    db.set_keep_ttl(args[1].clone(), Value::String(Bytes::from(buf)));
    Frame::Integer(old)
}

pub fn getbit(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() != 3 {
        return wrong_args("getbit");
    }
    let offset = match parse_int(&args[2]) {
        Ok(n) if n >= 0 && n < MAX_BIT_OFFSET => n as usize,
        _ => return Frame::err("bit offset is not an integer or out of range"),
    };
    match db.get_str(&args[1]) {
        Ok(Some(s)) => Frame::Integer(get_bit(s, offset) as i64),
        Ok(None) => Frame::Integer(0),
        Err(_) => Frame::wrongtype(),
    }
}

pub fn bitcount(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() != 2 && args.len() != 4 && args.len() != 5 {
        return Frame::err("syntax error");
    }
    let s = match db.get_str(&args[1]) {
        Ok(Some(s)) => s.clone(),
        Ok(None) => return Frame::Integer(0),
        Err(_) => return Frame::wrongtype(),
    };

    if args.len() == 2 {
        let count: u32 = s.iter().map(|b| b.count_ones()).sum();
        return Frame::Integer(count as i64);
    }

    let start = match parse_int(&args[2]) {
        Ok(n) => n,
        Err(e) => return e,
    };
    let end = match parse_int(&args[3]) {
        Ok(n) => n,
        Err(e) => return e,
    };
    let bit_unit = args.len() == 5 && upper(&args[4]) == "BIT";
    if args.len() == 5 && !bit_unit && upper(&args[4]) != "BYTE" {
        return Frame::err("syntax error");
    }

    let total_bits = s.len() * 8;
    let (lo, hi) = if bit_unit {
        match clamp(start, end, total_bits) {
            Some(r) => r,
            None => return Frame::Integer(0),
        }
    } else {
        match clamp(start, end, s.len()) {
            Some((a, b)) => (a * 8, b * 8 + 7),
            None => return Frame::Integer(0),
        }
    };
    let count = (lo..=hi.min(total_bits.saturating_sub(1)))
        .filter(|&b| get_bit(&s, b) == 1)
        .count();
    Frame::Integer(count as i64)
}

pub fn bitpos(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() < 3 || args.len() > 6 {
        return Frame::err("syntax error");
    }
    let target = match parse_int(&args[2]) {
        Ok(0) => 0u8,
        Ok(1) => 1u8,
        _ => return Frame::err("The bit argument must be 1 or 0."),
    };
    let s = match db.get_str(&args[1]) {
        Ok(Some(s)) => s.clone(),
        Ok(None) => return Frame::Integer(if target == 0 { 0 } else { -1 }),
        Err(_) => return Frame::wrongtype(),
    };

    let total_bits = s.len() * 8;
    let bit_unit = args.len() == 6 && upper(&args[5]) == "BIT";
    // Whether an explicit `end` index was supplied.
    let end_given = args.len() >= 5;

    // Resolve the search window in bit indices.
    let (lo, hi) = if args.len() == 3 {
        (0, total_bits.saturating_sub(1))
    } else {
        let start = match parse_int(&args[3]) {
            Ok(n) => n,
            Err(e) => return e,
        };
        let end = if end_given {
            match parse_int(&args[4]) {
                Ok(n) => n,
                Err(e) => return e,
            }
        } else {
            -1
        };
        let unit_len = if bit_unit { total_bits } else { s.len() };
        match clamp(start, end, unit_len) {
            Some((a, b)) if bit_unit => (a, b),
            Some((a, b)) => (a * 8, b * 8 + 7),
            None => return Frame::Integer(-1),
        }
    };

    let hi = hi.min(total_bits.saturating_sub(1));
    for b in lo..=hi {
        if get_bit(&s, b) == target {
            return Frame::Integer(b as i64);
        }
    }
    // Searching for a clear bit without an explicit end extends into the
    // implicit zeros past the string, so the answer is the first bit past it.
    if target == 0 && !end_given {
        return Frame::Integer(total_bits as i64);
    }
    Frame::Integer(-1)
}

pub fn bitop(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() < 4 {
        return wrong_args("bitop");
    }
    let op = upper(&args[1]);
    let dest = &args[2];
    let keys = &args[3..];

    if op == "NOT" && keys.len() != 1 {
        return Frame::err("BITOP NOT must be called with a single source key.");
    }

    let mut sources: Vec<Vec<u8>> = Vec::with_capacity(keys.len());
    for k in keys {
        match db.get_str(k) {
            Ok(Some(s)) => sources.push(s.to_vec()),
            Ok(None) => sources.push(Vec::new()),
            Err(_) => return Frame::wrongtype(),
        }
    }
    let maxlen = sources.iter().map(|s| s.len()).max().unwrap_or(0);

    let result: Vec<u8> = match op.as_str() {
        "AND" => (0..maxlen)
            .map(|i| {
                sources
                    .iter()
                    .fold(0xffu8, |acc, s| acc & s.get(i).copied().unwrap_or(0))
            })
            .collect(),
        "OR" => (0..maxlen)
            .map(|i| {
                sources
                    .iter()
                    .fold(0u8, |acc, s| acc | s.get(i).copied().unwrap_or(0))
            })
            .collect(),
        "XOR" => (0..maxlen)
            .map(|i| {
                sources
                    .iter()
                    .fold(0u8, |acc, s| acc ^ s.get(i).copied().unwrap_or(0))
            })
            .collect(),
        "NOT" => sources[0].iter().map(|b| !b).collect(),
        _ => return Frame::err("syntax error"),
    };

    let len = result.len();
    if len == 0 {
        db.remove(dest);
    } else {
        db.set(dest.clone(), Value::String(Bytes::from(result)));
    }
    Frame::Integer(len as i64)
}

/// Normalize an inclusive `[start, end]` range with Redis negative-index
/// semantics against a length. Returns `None` if the range is empty.
fn clamp(start: i64, end: i64, len: usize) -> Option<(usize, usize)> {
    if len == 0 {
        return None;
    }
    let len = len as i64;
    let mut s = if start < 0 { start + len } else { start };
    let mut e = if end < 0 { end + len } else { end };
    if s < 0 {
        s = 0;
    }
    if e >= len {
        e = len - 1;
    }
    if s > e || s >= len {
        return None;
    }
    Some((s as usize, e as usize))
}
