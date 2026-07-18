//! Generic keyspace commands: existence, expiry, renaming, scanning.

use super::{parse_int, rand_u64, upper, wrong_args};
use crate::db::{now_ms, Db};
use crate::resp::Frame;
use bytes::Bytes;

pub fn del(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() < 2 {
        return wrong_args("del");
    }
    let n = args[1..].iter().filter(|k| db.remove(k)).count();
    Frame::Integer(n as i64)
}

pub fn exists(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() < 2 {
        return wrong_args("exists");
    }
    // Repeated keys are counted repeatedly, matching Redis.
    let n = args[1..].iter().filter(|k| db.contains(k)).count();
    Frame::Integer(n as i64)
}

/// EXPIRE/PEXPIRE (relative) and EXPIREAT/PEXPIREAT (absolute). `unit_ms` is
/// the millisecond multiplier for the numeric argument.
pub fn expire(db: &mut Db, args: &[Bytes], unit_ms: i64, relative: bool) -> Frame {
    if args.len() < 3 {
        return wrong_args("expire");
    }
    let n = match parse_int(&args[2]) {
        Ok(n) => n,
        Err(e) => return e,
    };

    let mut nx = false;
    let mut xx = false;
    let mut gt = false;
    let mut lt = false;
    for opt in &args[3..] {
        match upper(opt).as_str() {
            "NX" => nx = true,
            "XX" => xx = true,
            "GT" => gt = true,
            "LT" => lt = true,
            _ => return Frame::err("Unsupported option"),
        }
    }
    if (nx && (xx || gt || lt)) || (gt && lt) {
        return Frame::err("NX and XX, GT or LT options at the same time are not compatible");
    }

    if !db.contains(&args[1]) {
        return Frame::Integer(0);
    }

    let now = now_ms() as i128;
    let at = if relative {
        now + n as i128 * unit_ms as i128
    } else {
        n as i128 * unit_ms as i128
    };

    let current = db.expire_at(&args[1]).map(|v| v as i128);
    let ok = if nx {
        current.is_none()
    } else if xx {
        current.is_some()
    } else if gt {
        // No current TTL is treated as infinite, so GT can never apply.
        matches!(current, Some(c) if at > c)
    } else if lt {
        // No current TTL is infinite, so LT always applies.
        current.map_or(true, |c| at < c)
    } else {
        true
    };
    if !ok {
        return Frame::Integer(0);
    }

    if at <= now {
        db.remove(&args[1]);
    } else {
        let at = at.min(u64::MAX as i128) as u64;
        db.set_expire(&args[1], at);
    }
    Frame::Integer(1)
}

pub fn ttl(db: &mut Db, args: &[Bytes], seconds: bool) -> Frame {
    if args.len() != 2 {
        return wrong_args("ttl");
    }
    if !db.contains(&args[1]) {
        return Frame::Integer(-2);
    }
    match db.expire_at(&args[1]) {
        None => Frame::Integer(-1),
        Some(at) => {
            let remaining = at.saturating_sub(now_ms());
            if seconds {
                Frame::Integer(((remaining + 500) / 1000) as i64)
            } else {
                Frame::Integer(remaining as i64)
            }
        }
    }
}

pub fn expiretime(db: &mut Db, args: &[Bytes], seconds: bool) -> Frame {
    if args.len() != 2 {
        return wrong_args("expiretime");
    }
    if !db.contains(&args[1]) {
        return Frame::Integer(-2);
    }
    match db.expire_at(&args[1]) {
        None => Frame::Integer(-1),
        Some(at) => Frame::Integer(if seconds {
            (at / 1000) as i64
        } else {
            at as i64
        }),
    }
}

pub fn persist(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() != 2 {
        return wrong_args("persist");
    }
    Frame::Integer(db.persist(&args[1]) as i64)
}

pub fn keys(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() != 2 {
        return wrong_args("keys");
    }
    let matched = db.keys_matching(Some(&args[1]));
    Frame::Array(matched.into_iter().map(Frame::Bulk).collect())
}

/// SCAN cursor [MATCH pattern] [COUNT n] [TYPE t]. We return the entire live
/// keyspace (filtered) in a single pass with a terminal cursor of "0", which
/// is a valid SCAN result and keeps client scan-loops working.
pub fn scan(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() < 2 {
        return wrong_args("scan");
    }
    let mut pattern: Option<Bytes> = None;
    let mut type_filter: Option<String> = None;
    let mut i = 2;
    while i < args.len() {
        match upper(&args[i]).as_str() {
            "MATCH" if i + 1 < args.len() => {
                pattern = Some(args[i + 1].clone());
                i += 2;
            }
            "COUNT" if i + 1 < args.len() => {
                if parse_int(&args[i + 1]).is_err() {
                    return Frame::err("value is not an integer or out of range");
                }
                i += 2;
            }
            "TYPE" if i + 1 < args.len() => {
                type_filter = Some(upper(&args[i + 1]).to_lowercase());
                i += 2;
            }
            _ => return Frame::err("syntax error"),
        }
    }

    let mut keys = db.keys_matching(pattern.as_deref());
    if let Some(t) = type_filter {
        keys.retain(|k| db.get(k).map(|v| v.type_name()) == Some(t.as_str()));
    }
    Frame::Array(vec![
        Frame::bulk("0"),
        Frame::Array(keys.into_iter().map(Frame::Bulk).collect()),
    ])
}

pub fn type_cmd(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() != 2 {
        return wrong_args("type");
    }
    let name = db.get(&args[1]).map(|v| v.type_name()).unwrap_or("none");
    Frame::Simple(name.into())
}

pub fn rename(db: &mut Db, args: &[Bytes], nx: bool) -> Frame {
    if args.len() != 3 {
        return wrong_args(if nx { "renamenx" } else { "rename" });
    }
    if !db.contains(&args[1]) {
        return Frame::err("no such key");
    }
    if nx && db.contains(&args[2]) {
        return Frame::Integer(0);
    }
    db.rename(&args[1], args[2].clone());
    if nx {
        Frame::Integer(1)
    } else {
        Frame::ok()
    }
}

pub fn randomkey(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() != 1 {
        return wrong_args("randomkey");
    }
    let keys = db.all_keys();
    if keys.is_empty() {
        Frame::Null
    } else {
        let idx = (rand_u64() % keys.len() as u64) as usize;
        Frame::Bulk(keys[idx].clone())
    }
}

pub fn copy(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() < 3 {
        return wrong_args("copy");
    }
    let mut replace = false;
    let mut i = 3;
    while i < args.len() {
        match upper(&args[i]).as_str() {
            "REPLACE" => {
                replace = true;
                i += 1;
            }
            // DB targeting is meaningless here (single database); accept & skip.
            "DB" if i + 1 < args.len() => i += 2,
            _ => return Frame::err("syntax error"),
        }
    }

    if !db.contains(&args[1]) {
        return Frame::Integer(0);
    }
    if db.contains(&args[2]) && !replace {
        return Frame::Integer(0);
    }
    let value = match db.get(&args[1]).cloned() {
        Some(v) => v,
        None => return Frame::Integer(0),
    };
    let ttl = db.expire_at(&args[1]);
    db.set(args[2].clone(), value);
    if let Some(at) = ttl {
        db.set_expire(&args[2], at);
    }
    Frame::Integer(1)
}
