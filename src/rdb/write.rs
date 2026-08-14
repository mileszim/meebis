//! Writing a [`Keyspace`] out as an RDB file.
//!
//! Where the reader has to accept every encoding Redis might emit, the writer
//! gets to choose, and it always chooses the flat one: a list is a list of
//! strings, a hash is a run of field/value pairs. Real Redis has not written
//! those forms for years — it packs small collections into listpacks — but it
//! still *loads* them, which is the only thing that matters here. That single
//! decision removes the entire packed-encoding writer from this file.
//!
//! Streams are the exception, and live in [`super::stream`].

use super::stream;
use crate::db::{Keyspace, Value};

/// Type codes meebis writes. Deliberately the oldest, simplest spelling of each
/// type that current Redis still accepts.
const STRING: u8 = 0;
const LIST: u8 = 1;
const SET: u8 = 2;
const HASH: u8 = 4;
const ZSET_2: u8 = 5;

const OP_AUX: u8 = 0xfa;
const OP_RESIZEDB: u8 = 0xfb;
const OP_EXPIRETIME_MS: u8 = 0xfc;
const OP_SELECTDB: u8 = 0xfe;
const OP_EOF: u8 = 0xff;

/// The RDB version stamped into the header. Matches the `redis_version:7.4.0`
/// meebis reports in `INFO`, and is the version the local Redis 7.2+ toolchain
/// writes. Redis refuses a file whose version exceeds its own, so this is also
/// the floor on which Redis releases can read what meebis produces (7.2+).
const RDB_VERSION: u32 = 11;

/// Serialize the whole keyspace.
pub fn to_bytes(ks: &mut Keyspace) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(format!("REDIS{RDB_VERSION:04}").as_bytes());

    put_aux(&mut out, b"redis-ver", b"7.4.0");
    put_aux(&mut out, b"redis-bits", b"64");
    put_aux(
        &mut out,
        b"meebis-ver",
        env!("CARGO_PKG_VERSION").as_bytes(),
    );

    for index in 0..ks.len() {
        // Skip empty databases entirely, exactly as Redis does — an untouched
        // 16-database server should not produce 16 section headers.
        if ks.db(index).len() == 0 {
            continue;
        }

        out.push(OP_SELECTDB);
        put_length(&mut out, index as u64);

        let db = ks.db(index);
        out.push(OP_RESIZEDB);
        put_length(&mut out, db.len() as u64);
        put_length(&mut out, db.expires_count() as u64);

        for (key, value, expire_at) in db.iter_live() {
            if let Some(at) = expire_at {
                out.push(OP_EXPIRETIME_MS);
                out.extend_from_slice(&at.to_le_bytes());
            }
            put_value(&mut out, key, value);
        }
    }

    out.push(OP_EOF);
    let checksum = super::crc64::checksum(&out);
    out.extend_from_slice(&checksum.to_le_bytes());
    out
}

fn put_aux(out: &mut Vec<u8>, key: &[u8], value: &[u8]) {
    out.push(OP_AUX);
    put_raw_string(out, key);
    put_raw_string(out, value);
}

/// Write a key and its value, type byte first.
fn put_value(out: &mut Vec<u8>, key: &[u8], value: &Value) {
    match value {
        Value::String(s) => {
            out.push(STRING);
            put_raw_string(out, key);
            put_raw_string(out, s);
        }
        Value::List(items) => {
            out.push(LIST);
            put_raw_string(out, key);
            put_length(out, items.len() as u64);
            for item in items {
                put_raw_string(out, item);
            }
        }
        Value::Set(members) => {
            out.push(SET);
            put_raw_string(out, key);
            put_length(out, members.len() as u64);
            for member in members {
                put_raw_string(out, member);
            }
        }
        Value::Hash(fields) => {
            out.push(HASH);
            put_raw_string(out, key);
            put_length(out, fields.len() as u64);
            for (field, v) in fields {
                put_raw_string(out, field);
                put_raw_string(out, v);
            }
        }
        Value::ZSet(z) => {
            out.push(ZSET_2);
            put_raw_string(out, key);
            put_length(out, z.len() as u64);
            for (member, score) in z.iter_asc() {
                put_raw_string(out, member);
                // ZSET_2 stores the raw double rather than its decimal form,
                // which is what keeps inf/-inf and full precision intact.
                out.extend_from_slice(&score.to_le_bytes());
            }
        }
        Value::Stream(s) => {
            out.push(stream::WRITE_TYPE);
            put_raw_string(out, key);
            stream::write(out, s);
        }
    }
}

/// The RDB length encoding, narrowest form first.
pub fn put_length(out: &mut Vec<u8>, n: u64) {
    if n < 64 {
        out.push(n as u8);
    } else if n < 16384 {
        out.push(0x40 | ((n >> 8) as u8));
        out.push((n & 0xff) as u8);
    } else if n <= u32::MAX as u64 {
        out.push(0x80);
        out.extend_from_slice(&(n as u32).to_be_bytes());
    } else {
        out.push(0x81);
        out.extend_from_slice(&n.to_be_bytes());
    }
}

/// A length-prefixed string. meebis never compresses and never uses the
/// integer encodings: both are optional on the way out, and skipping them
/// keeps this side of the codec free of choices that could go wrong.
pub fn put_raw_string(out: &mut Vec<u8>, s: &[u8]) {
    put_length(out, s.len() as u64);
    out.extend_from_slice(s);
}
