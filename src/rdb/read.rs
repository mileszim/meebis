//! Reading an RDB file into a [`Keyspace`].
//!
//! Unlike writing — where meebis picks the simplest legal encoding for every
//! type — reading has to accept whatever real Redis chose to emit. A dump from
//! Redis 7.2 uses a listpack for every small collection, an intset for a set of
//! small integers, and a quicklist for *every* list, however short. Older dumps
//! add the ziplist spellings of the same ideas.
//!
//! The whole file is read into memory before parsing so the trailing CRC can be
//! checked against the bytes that produced it. These are dev-machine snapshots,
//! not multi-gigabyte production dumps.

use super::cursor::{Len, Reader};
use super::{packed, stream, Error, LoadStats, Result};
use crate::db::{now_ms, Keyspace, Value, ZSet};
use std::collections::{HashMap, HashSet, VecDeque};

// Value type codes. Names mirror Redis' `RDB_TYPE_*` so the two can be diffed
// against each other by eye.
const STRING: u8 = 0;
const LIST: u8 = 1;
const SET: u8 = 2;
const ZSET: u8 = 3;
const HASH: u8 = 4;
const ZSET_2: u8 = 5;
const MODULE_PRE_GA: u8 = 6;
const MODULE_2: u8 = 7;
const HASH_ZIPMAP: u8 = 9;
const LIST_ZIPLIST: u8 = 10;
const SET_INTSET: u8 = 11;
const ZSET_ZIPLIST: u8 = 12;
const HASH_ZIPLIST: u8 = 13;
const LIST_QUICKLIST: u8 = 14;
const STREAM_LISTPACKS: u8 = 15;
const HASH_LISTPACK: u8 = 16;
const ZSET_LISTPACK: u8 = 17;
const LIST_QUICKLIST_2: u8 = 18;
const STREAM_LISTPACKS_2: u8 = 19;
const SET_LISTPACK: u8 = 20;
const STREAM_LISTPACKS_3: u8 = 21;
const HASH_METADATA_PRE_GA: u8 = 22;
const HASH_LISTPACK_EX_PRE_GA: u8 = 23;
const HASH_METADATA: u8 = 24;
const HASH_LISTPACK_EX: u8 = 25;

// File-level opcodes.
const OP_SLOT_INFO: u8 = 0xf4;
const OP_FUNCTION2: u8 = 0xf5;
const OP_FUNCTION_PRE_GA: u8 = 0xf6;
const OP_MODULE_AUX: u8 = 0xf7;
const OP_IDLE: u8 = 0xf8;
const OP_FREQ: u8 = 0xf9;
const OP_AUX: u8 = 0xfa;
const OP_RESIZEDB: u8 = 0xfb;
const OP_EXPIRETIME_MS: u8 = 0xfc;
const OP_EXPIRETIME: u8 = 0xfd;
const OP_SELECTDB: u8 = 0xfe;
const OP_EOF: u8 = 0xff;

/// Quicklist node containers: a `PLAIN` node holds one raw element, a `PACKED`
/// node holds a listpack (or, in the v1 encoding, a ziplist).
const QUICKLIST_PLAIN: u64 = 1;
const QUICKLIST_PACKED: u64 = 2;

/// The newest RDB version meebis understands. Redis refuses files newer than
/// itself and so do we — a higher version can contain encodings that did not
/// exist when this was written, and guessing at them corrupts data silently.
pub const MAX_RDB_VERSION: u32 = 11;

/// Parse a complete RDB image into `ks`, which callers are expected to hand
/// over empty.
pub fn from_bytes(buf: &[u8], ks: &mut Keyspace) -> Result<LoadStats> {
    if buf.len() < 9 || &buf[..5] != b"REDIS" {
        return Err(Error::Corrupt("not an RDB file (bad magic)".into()));
    }
    let version: u32 = std::str::from_utf8(&buf[5..9])
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| Error::Corrupt("unparseable RDB version".into()))?;
    if version > MAX_RDB_VERSION {
        return Err(Error::Unsupported(format!(
            "RDB version {version} is newer than the {MAX_RDB_VERSION} meebis understands"
        )));
    }

    verify_checksum(buf, version)?;

    let mut r = Reader::new(buf);
    r.seek(9);
    let mut stats = LoadStats::default();
    let mut db_index = 0usize;
    // An expiry applies to the next key/value pair and is consumed by it.
    // Crucially it survives the opcodes that can sit *between* the two: Redis
    // writes the expiry first, then any LRU/LFU metadata, then the key, so
    // clearing this on anything but a key would silently drop TTLs from a dump
    // taken with an eviction policy set.
    let mut pending_expire: Option<u64> = None;

    loop {
        let op = r.byte()?;
        match op {
            OP_EOF => break,
            OP_SELECTDB => {
                let n = r.count()?;
                if !ks.is_valid(n as i64) {
                    return Err(Error::Unsupported(format!(
                        "dump selects database {n} but this server has only {} \
                         (restart meebis with --databases {})",
                        ks.len(),
                        n + 1
                    )));
                }
                db_index = n;
                pending_expire = None;
            }
            OP_RESIZEDB => {
                // Sizing hints for Redis' hash tables; nothing to preallocate.
                r.count()?;
                r.count()?;
            }
            OP_AUX => {
                let key = r.string()?;
                let value = r.string()?;
                stats.aux.push((key, value));
            }
            OP_EXPIRETIME_MS => pending_expire = Some(r.u64le()?),
            OP_EXPIRETIME => pending_expire = Some(r.u32le()? as u64 * 1000),
            // Eviction bookkeeping meebis does not model.
            OP_IDLE => {
                r.count()?;
            }
            OP_FREQ => {
                r.byte()?;
            }
            OP_FUNCTION2 | OP_FUNCTION_PRE_GA => {
                // A library's source, which meebis has nowhere to put — it
                // implements EVAL but not FUNCTION.
                r.string()?;
                stats.dropped_functions += 1;
            }
            OP_SLOT_INFO => {
                // Cluster slot bookkeeping; irrelevant to a standalone server.
                r.count()?;
                r.count()?;
                r.count()?;
            }
            OP_MODULE_AUX => {
                return Err(Error::Unsupported(
                    "dump contains module data, which meebis cannot interpret".into(),
                ))
            }
            type_code => {
                let key = r.string()?;
                let value = read_value(&mut r, type_code, &mut stats)?;
                let expire_at = pending_expire.take();

                // Redis discards already-expired keys when loading as a master,
                // rather than materializing them for the sweeper to find.
                if matches!(expire_at, Some(at) if at <= now_ms()) {
                    stats.expired += 1;
                    continue;
                }
                ks.db(db_index).put(key, value, expire_at);
                stats.keys += 1;
            }
        }
    }

    Ok(stats)
}

/// Check the 8-byte CRC64 trailer. Version 5 introduced it, and a zero value
/// means the writer had checksums turned off — Redis accepts that, so we do.
fn verify_checksum(buf: &[u8], version: u32) -> Result<()> {
    if version < 5 || buf.len() < 8 {
        return Ok(());
    }
    let split = buf.len() - 8;
    let stored = u64::from_le_bytes(buf[split..].try_into().unwrap());
    if stored == 0 {
        return Ok(());
    }
    let computed = super::crc64::checksum(&buf[..split]);
    if stored != computed {
        return Err(Error::Corrupt(format!(
            "checksum mismatch: file says {stored:#018x}, contents hash to {computed:#018x}"
        )));
    }
    Ok(())
}

/// Read one value of the given type code.
fn read_value(r: &mut Reader, type_code: u8, stats: &mut LoadStats) -> Result<Value> {
    match type_code {
        STRING => Ok(Value::String(r.string()?)),

        LIST => {
            let n = r.count()?;
            let mut list = VecDeque::with_capacity(n.min(1024));
            for _ in 0..n {
                list.push_back(r.string()?);
            }
            Ok(Value::List(list))
        }

        LIST_ZIPLIST => {
            let blob = r.string()?;
            let items = packed::ziplist_strings(&blob)
                .ok_or_else(|| r.corrupt("malformed ziplist in list"))?;
            Ok(Value::List(items.into()))
        }

        // Both quicklist generations are a list of nodes; only the packed
        // node's inner format differs (ziplist in v1, listpack in v2).
        LIST_QUICKLIST | LIST_QUICKLIST_2 => {
            let nodes = r.count()?;
            let mut list = VecDeque::new();
            for _ in 0..nodes {
                // The v1 encoding has no container field: every node is packed.
                let container = if type_code == LIST_QUICKLIST_2 {
                    match r.length()? {
                        Len::Plain(c) => c,
                        Len::Special(_) => return Err(r.corrupt("bad quicklist container")),
                    }
                } else {
                    QUICKLIST_PACKED
                };
                let blob = r.string()?;
                match container {
                    QUICKLIST_PLAIN => list.push_back(blob),
                    QUICKLIST_PACKED => {
                        let items = if type_code == LIST_QUICKLIST_2 {
                            packed::listpack_strings(&blob)
                        } else {
                            packed::ziplist_strings(&blob)
                        }
                        .ok_or_else(|| r.corrupt("malformed quicklist node"))?;
                        list.extend(items);
                    }
                    other => {
                        return Err(Error::Corrupt(format!(
                            "unknown quicklist container {other}"
                        )))
                    }
                }
            }
            Ok(Value::List(list))
        }

        SET => {
            let n = r.count()?;
            let mut set = HashSet::with_capacity(n.min(1024));
            for _ in 0..n {
                set.insert(r.string()?);
            }
            Ok(Value::Set(set))
        }

        SET_INTSET => {
            let blob = r.string()?;
            let items = packed::intset(&blob).ok_or_else(|| r.corrupt("malformed intset"))?;
            Ok(Value::Set(items.into_iter().collect()))
        }

        SET_LISTPACK => {
            let blob = r.string()?;
            let items = packed::listpack_strings(&blob)
                .ok_or_else(|| r.corrupt("malformed listpack in set"))?;
            Ok(Value::Set(items.into_iter().collect()))
        }

        HASH => {
            let n = r.count()?;
            let mut hash = HashMap::with_capacity(n.min(1024));
            for _ in 0..n {
                let field = r.string()?;
                let value = r.string()?;
                hash.insert(field, value);
            }
            Ok(Value::Hash(hash))
        }

        HASH_ZIPLIST | HASH_LISTPACK => {
            let blob = r.string()?;
            let flat = if type_code == HASH_LISTPACK {
                packed::listpack_strings(&blob)
            } else {
                packed::ziplist_strings(&blob)
            }
            .ok_or_else(|| r.corrupt("malformed packed hash"))?;
            if flat.len() % 2 != 0 {
                return Err(r.corrupt("packed hash has an odd number of elements"));
            }
            let mut hash = HashMap::with_capacity(flat.len() / 2);
            let mut it = flat.into_iter();
            while let (Some(f), Some(v)) = (it.next(), it.next()) {
                hash.insert(f, v);
            }
            Ok(Value::Hash(hash))
        }

        ZSET | ZSET_2 => {
            let n = r.count()?;
            let mut zset = ZSet::new();
            for _ in 0..n {
                let member = r.string()?;
                let score = if type_code == ZSET_2 {
                    r.binary_double()?
                } else {
                    r.string_double()?
                };
                zset.insert(member, score);
            }
            Ok(Value::ZSet(zset))
        }

        ZSET_ZIPLIST | ZSET_LISTPACK => {
            let blob = r.string()?;
            let flat = if type_code == ZSET_LISTPACK {
                packed::listpack_strings(&blob)
            } else {
                packed::ziplist_strings(&blob)
            }
            .ok_or_else(|| r.corrupt("malformed packed zset"))?;
            if flat.len() % 2 != 0 {
                return Err(r.corrupt("packed zset has an odd number of elements"));
            }
            let mut zset = ZSet::new();
            let mut it = flat.into_iter();
            while let (Some(m), Some(s)) = (it.next(), it.next()) {
                let score = std::str::from_utf8(&s)
                    .ok()
                    .and_then(parse_score)
                    .ok_or_else(|| r.corrupt("unparseable packed zset score"))?;
                zset.insert(m, score);
            }
            Ok(Value::ZSet(zset))
        }

        STREAM_LISTPACKS | STREAM_LISTPACKS_2 | STREAM_LISTPACKS_3 => {
            stream::read(r, type_code, stats)
        }

        HASH_ZIPMAP => Err(Error::Unsupported(
            "dump uses the pre-2.6 zipmap hash encoding".into(),
        )),
        MODULE_PRE_GA | MODULE_2 => Err(Error::Unsupported(
            "dump contains a module type, which meebis cannot interpret".into(),
        )),
        HASH_METADATA | HASH_METADATA_PRE_GA | HASH_LISTPACK_EX | HASH_LISTPACK_EX_PRE_GA => {
            Err(Error::Unsupported(
                "dump contains a hash with per-field TTLs (Redis 7.4 HEXPIRE), \
                 which meebis does not model"
                    .into(),
            ))
        }
        other => Err(Error::Corrupt(format!("unknown value type {other}"))),
    }
}

/// Redis renders non-finite scores as literals inside packed encodings.
/// `f64::from_str` handles `inf`/`-inf`/`nan`, but not the `+inf` spelling.
fn parse_score(s: &str) -> Option<f64> {
    match s {
        "inf" | "+inf" => Some(f64::INFINITY),
        "-inf" => Some(f64::NEG_INFINITY),
        "nan" => Some(f64::NAN),
        other => other.parse().ok(),
    }
}
