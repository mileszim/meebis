//! String commands: GET/SET and friends, counters, and range ops.

use super::{parse_float, parse_int, upper, wrong_args};
use crate::db::{now_ms, Db, Value};
use crate::resp::{format_double, Frame};
use bytes::Bytes;

pub fn get(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() != 2 {
        return wrong_args("get");
    }
    match db.get_str(&args[1]) {
        Ok(Some(s)) => Frame::Bulk(s.clone()),
        Ok(None) => Frame::Null,
        Err(_) => Frame::wrongtype(),
    }
}

pub fn set(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() < 3 {
        return wrong_args("set");
    }
    let key = &args[1];
    let value = args[2].clone();

    let mut nx = false;
    let mut xx = false;
    let mut get = false;
    let mut keepttl = false;
    // Absolute expiry in unix ms, if an EX/PX/EXAT/PXAT option was given.
    let mut expire_at: Option<u64> = None;
    let mut expire_seen = false;

    let mut i = 3;
    while i < args.len() {
        match upper(&args[i]).as_str() {
            "NX" => nx = true,
            "XX" => xx = true,
            "GET" => get = true,
            "KEEPTTL" => keepttl = true,
            opt @ ("EX" | "PX" | "EXAT" | "PXAT") => {
                if expire_seen || keepttl {
                    return Frame::err("syntax error");
                }
                expire_seen = true;
                i += 1;
                let n = match args.get(i) {
                    Some(a) => match parse_int(a) {
                        Ok(n) => n,
                        Err(e) => return e,
                    },
                    None => return Frame::err("syntax error"),
                };
                let now = now_ms();
                expire_at = Some(match opt {
                    "EX" => {
                        if n <= 0 {
                            return invalid_expire("set");
                        }
                        now + n as u64 * 1000
                    }
                    "PX" => {
                        if n <= 0 {
                            return invalid_expire("set");
                        }
                        now + n as u64
                    }
                    "EXAT" => n as u64 * 1000,
                    _ => n as u64,
                });
            }
            _ => return Frame::err("syntax error"),
        }
        i += 1;
    }

    if (nx && xx) || (keepttl && expire_seen) {
        return Frame::err("syntax error");
    }

    // Fetch the old value if GET was requested (must be a string).
    let old = if get {
        match db.get_str(key) {
            Ok(Some(s)) => Some(s.clone()),
            Ok(None) => None,
            Err(_) => return Frame::wrongtype(),
        }
    } else {
        None
    };

    let exists = db.contains(key);
    let should_set = !(nx && exists) && !(xx && !exists);

    if should_set {
        if keepttl {
            db.set_keep_ttl(key.clone(), Value::String(value));
        } else {
            db.set(key.clone(), Value::String(value));
            if let Some(at) = expire_at {
                db.set_expire(key, at);
            }
        }
    }

    if get {
        match old {
            Some(s) => Frame::Bulk(s),
            None => Frame::Null,
        }
    } else if should_set {
        Frame::ok()
    } else {
        Frame::Null
    }
}

pub fn setnx(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() != 3 {
        return wrong_args("setnx");
    }
    if db.contains(&args[1]) {
        Frame::Integer(0)
    } else {
        db.set(args[1].clone(), Value::String(args[2].clone()));
        Frame::Integer(1)
    }
}

pub fn setex(db: &mut Db, args: &[Bytes], ms: bool) -> Frame {
    let name = if ms { "psetex" } else { "setex" };
    if args.len() != 4 {
        return wrong_args(name);
    }
    let n = match parse_int(&args[2]) {
        Ok(n) => n,
        Err(e) => return e,
    };
    if n <= 0 {
        return invalid_expire(name);
    }
    let at = now_ms() + if ms { n as u64 } else { n as u64 * 1000 };
    db.set(args[1].clone(), Value::String(args[3].clone()));
    db.set_expire(&args[1], at);
    Frame::ok()
}

pub fn getset(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() != 3 {
        return wrong_args("getset");
    }
    let old = match db.get_str(&args[1]) {
        Ok(Some(s)) => Frame::Bulk(s.clone()),
        Ok(None) => Frame::Null,
        Err(_) => return Frame::wrongtype(),
    };
    db.set(args[1].clone(), Value::String(args[2].clone()));
    old
}

pub fn getdel(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() != 2 {
        return wrong_args("getdel");
    }
    match db.get_str(&args[1]) {
        Ok(Some(s)) => {
            let v = s.clone();
            db.remove(&args[1]);
            Frame::Bulk(v)
        }
        Ok(None) => Frame::Null,
        Err(_) => Frame::wrongtype(),
    }
}

pub fn getex(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() < 2 {
        return wrong_args("getex");
    }
    // Read the value first (WRONGTYPE if not a string).
    let value = match db.get_str(&args[1]) {
        Ok(Some(s)) => s.clone(),
        Ok(None) => return Frame::Null,
        Err(_) => return Frame::wrongtype(),
    };
    let now = now_ms();
    let mut i = 2;
    while i < args.len() {
        match upper(&args[i]).as_str() {
            "PERSIST" => {
                db.persist(&args[1]);
            }
            opt @ ("EX" | "PX" | "EXAT" | "PXAT") => {
                i += 1;
                let n = match args.get(i) {
                    Some(a) => match parse_int(a) {
                        Ok(n) => n,
                        Err(e) => return e,
                    },
                    None => return Frame::err("syntax error"),
                };
                let at = match opt {
                    "EX" => now + n as u64 * 1000,
                    "PX" => now + n as u64,
                    "EXAT" => n as u64 * 1000,
                    _ => n as u64,
                };
                db.set_expire(&args[1], at);
            }
            _ => return Frame::err("syntax error"),
        }
        i += 1;
    }
    Frame::Bulk(value)
}

pub fn append(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() != 3 {
        return wrong_args("append");
    }
    let mut buf = match db.get_str(&args[1]) {
        Ok(Some(s)) => s.to_vec(),
        Ok(None) => Vec::new(),
        Err(_) => return Frame::wrongtype(),
    };
    buf.extend_from_slice(&args[2]);
    let len = buf.len();
    db.set_keep_ttl(args[1].clone(), Value::String(Bytes::from(buf)));
    Frame::Integer(len as i64)
}

pub fn strlen(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() != 2 {
        return wrong_args("strlen");
    }
    match db.get_str(&args[1]) {
        Ok(Some(s)) => Frame::Integer(s.len() as i64),
        Ok(None) => Frame::Integer(0),
        Err(_) => Frame::wrongtype(),
    }
}

pub fn incr(db: &mut Db, args: &[Bytes], delta: i64) -> Frame {
    if args.len() != 2 {
        return wrong_args("incr");
    }
    do_incr(db, &args[1], delta)
}

pub fn incrby(db: &mut Db, args: &[Bytes], negate: bool) -> Frame {
    if args.len() != 3 {
        return wrong_args(if negate { "decrby" } else { "incrby" });
    }
    let n = match parse_int(&args[2]) {
        Ok(n) => n,
        Err(e) => return e,
    };
    let delta = if negate {
        match n.checked_neg() {
            Some(d) => d,
            None => return Frame::err("decrement would overflow"),
        }
    } else {
        n
    };
    do_incr(db, &args[1], delta)
}

fn do_incr(db: &mut Db, key: &Bytes, delta: i64) -> Frame {
    let cur = match db.get_str(key) {
        Ok(Some(s)) => match std::str::from_utf8(s)
            .ok()
            .and_then(|v| v.parse::<i64>().ok())
        {
            Some(n) => n,
            None => return Frame::err("value is not an integer or out of range"),
        },
        Ok(None) => 0,
        Err(_) => return Frame::wrongtype(),
    };
    match cur.checked_add(delta) {
        Some(next) => {
            db.set_keep_ttl(key.clone(), Value::String(Bytes::from(next.to_string())));
            Frame::Integer(next)
        }
        None => Frame::err("increment or decrement would overflow"),
    }
}

pub fn incrbyfloat(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() != 3 {
        return wrong_args("incrbyfloat");
    }
    let incr = match parse_float(&args[2]) {
        Ok(f) => f,
        Err(e) => return e,
    };
    let cur = match db.get_str(&args[1]) {
        Ok(Some(s)) => match std::str::from_utf8(s)
            .ok()
            .and_then(|v| v.trim().parse::<f64>().ok())
        {
            Some(f) => f,
            None => return Frame::err("value is not a valid float"),
        },
        Ok(None) => 0.0,
        Err(_) => return Frame::wrongtype(),
    };
    let next = cur + incr;
    if next.is_nan() || next.is_infinite() {
        return Frame::err("increment would produce NaN or Infinity");
    }
    let s = format_double(next);
    db.set_keep_ttl(args[1].clone(), Value::String(Bytes::from(s.clone())));
    Frame::Bulk(Bytes::from(s))
}

pub fn mget(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() < 2 {
        return wrong_args("mget");
    }
    let out = args[1..]
        .iter()
        .map(|k| match db.get_str(k) {
            Ok(Some(s)) => Frame::Bulk(s.clone()),
            _ => Frame::Null,
        })
        .collect();
    Frame::Array(out)
}

pub fn mset(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() < 3 || args.len() % 2 != 1 {
        return wrong_args("mset");
    }
    let mut i = 1;
    while i + 1 < args.len() {
        db.set(args[i].clone(), Value::String(args[i + 1].clone()));
        i += 2;
    }
    Frame::ok()
}

pub fn msetnx(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() < 3 || args.len() % 2 != 1 {
        return wrong_args("msetnx");
    }
    let mut i = 1;
    while i + 1 < args.len() {
        if db.contains(&args[i]) {
            return Frame::Integer(0);
        }
        i += 2;
    }
    let mut i = 1;
    while i + 1 < args.len() {
        db.set(args[i].clone(), Value::String(args[i + 1].clone()));
        i += 2;
    }
    Frame::Integer(1)
}

pub fn getrange(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() != 4 {
        return wrong_args("getrange");
    }
    let (start, end) = match (parse_int(&args[2]), parse_int(&args[3])) {
        (Ok(s), Ok(e)) => (s, e),
        (Err(e), _) | (_, Err(e)) => return e,
    };
    let s = match db.get_str(&args[1]) {
        Ok(Some(s)) => s.clone(),
        Ok(None) => return Frame::Bulk(Bytes::new()),
        Err(_) => return Frame::wrongtype(),
    };
    let len = s.len() as i64;
    if len == 0 {
        return Frame::Bulk(Bytes::new());
    }
    let mut start = if start < 0 { start + len } else { start };
    let mut end = if end < 0 { end + len } else { end };
    if start < 0 {
        start = 0;
    }
    if end < 0 {
        return Frame::Bulk(Bytes::new());
    }
    if end >= len {
        end = len - 1;
    }
    if start > end {
        return Frame::Bulk(Bytes::new());
    }
    Frame::Bulk(s.slice(start as usize..=end as usize))
}

pub fn setrange(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() != 4 {
        return wrong_args("setrange");
    }
    let offset = match parse_int(&args[2]) {
        Ok(n) if n >= 0 => n as usize,
        Ok(_) => return Frame::err("offset is out of range"),
        Err(e) => return e,
    };
    let patch = &args[3];
    let mut buf = match db.get_str(&args[1]) {
        Ok(Some(s)) => s.to_vec(),
        Ok(None) => Vec::new(),
        Err(_) => return Frame::wrongtype(),
    };
    if patch.is_empty() {
        return Frame::Integer(buf.len() as i64);
    }
    if buf.len() < offset + patch.len() {
        buf.resize(offset + patch.len(), 0);
    }
    buf[offset..offset + patch.len()].copy_from_slice(patch);
    let len = buf.len();
    db.set_keep_ttl(args[1].clone(), Value::String(Bytes::from(buf)));
    Frame::Integer(len as i64)
}

fn invalid_expire(cmd: &str) -> Frame {
    Frame::err(format!("invalid expire time in '{}' command", cmd))
}
