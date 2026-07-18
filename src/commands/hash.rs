//! Hash commands.

use super::{parse_float, parse_int, rand_u64, upper, wrong_args};
use crate::db::Db;
use crate::resp::{format_double, Frame};
use bytes::Bytes;

pub fn hset(db: &mut Db, args: &[Bytes], hmset: bool) -> Frame {
    let name = if hmset { "hmset" } else { "hset" };
    if args.len() < 4 || args.len() % 2 != 0 {
        return wrong_args(name);
    }
    let h = match db.get_or_create_hash(args[1].clone()) {
        Ok(h) => h,
        Err(_) => return Frame::wrongtype(),
    };
    let mut added = 0i64;
    let mut i = 2;
    while i + 1 < args.len() {
        if h.insert(args[i].clone(), args[i + 1].clone()).is_none() {
            added += 1;
        }
        i += 2;
    }
    if hmset {
        Frame::ok()
    } else {
        Frame::Integer(added)
    }
}

pub fn hsetnx(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() != 4 {
        return wrong_args("hsetnx");
    }
    let h = match db.get_or_create_hash(args[1].clone()) {
        Ok(h) => h,
        Err(_) => return Frame::wrongtype(),
    };
    // A newly created hash is empty, so `contains_key` is only ever true for a
    // pre-existing key — no empty-key cleanup is needed here.
    if h.contains_key(&args[2]) {
        Frame::Integer(0)
    } else {
        h.insert(args[2].clone(), args[3].clone());
        Frame::Integer(1)
    }
}

pub fn hget(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() != 3 {
        return wrong_args("hget");
    }
    match db.get_hash(&args[1]) {
        Ok(Some(h)) => match h.get(&args[2]) {
            Some(v) => Frame::Bulk(v.clone()),
            None => Frame::Null,
        },
        Ok(None) => Frame::Null,
        Err(_) => Frame::wrongtype(),
    }
}

pub fn hmget(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() < 3 {
        return wrong_args("hmget");
    }
    match db.get_hash(&args[1]) {
        Ok(Some(h)) => Frame::Array(
            args[2..]
                .iter()
                .map(|f| match h.get(f) {
                    Some(v) => Frame::Bulk(v.clone()),
                    None => Frame::Null,
                })
                .collect(),
        ),
        Ok(None) => Frame::Array(args[2..].iter().map(|_| Frame::Null).collect()),
        Err(_) => Frame::wrongtype(),
    }
}

pub fn hdel(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() < 3 {
        return wrong_args("hdel");
    }
    // Check type/existence first; the borrow ends before we take a mutable one.
    match db.get_hash(&args[1]) {
        Ok(Some(_)) => {}
        Ok(None) => return Frame::Integer(0),
        Err(_) => return Frame::wrongtype(),
    }
    let h = match db.get_or_create_hash(args[1].clone()) {
        Ok(h) => h,
        Err(_) => return Frame::wrongtype(),
    };
    let removed = args[2..].iter().filter(|f| h.remove(*f).is_some()).count();
    db.remove_if_empty(&args[1]);
    Frame::Integer(removed as i64)
}

pub fn hgetall(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() != 2 {
        return wrong_args("hgetall");
    }
    match db.get_hash(&args[1]) {
        Ok(Some(h)) => Frame::Map(
            h.iter()
                .map(|(k, v)| (Frame::Bulk(k.clone()), Frame::Bulk(v.clone())))
                .collect(),
        ),
        Ok(None) => Frame::Map(vec![]),
        Err(_) => Frame::wrongtype(),
    }
}

pub fn hkeys(db: &mut Db, args: &[Bytes]) -> Frame {
    hcollect(db, args, "hkeys", |k, _| Frame::Bulk(k.clone()))
}

pub fn hvals(db: &mut Db, args: &[Bytes]) -> Frame {
    hcollect(db, args, "hvals", |_, v| Frame::Bulk(v.clone()))
}

fn hcollect(db: &mut Db, args: &[Bytes], name: &str, f: impl Fn(&Bytes, &Bytes) -> Frame) -> Frame {
    if args.len() != 2 {
        return wrong_args(name);
    }
    match db.get_hash(&args[1]) {
        Ok(Some(h)) => Frame::Array(h.iter().map(|(k, v)| f(k, v)).collect()),
        Ok(None) => Frame::Array(vec![]),
        Err(_) => Frame::wrongtype(),
    }
}

pub fn hlen(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() != 2 {
        return wrong_args("hlen");
    }
    match db.get_hash(&args[1]) {
        Ok(Some(h)) => Frame::Integer(h.len() as i64),
        Ok(None) => Frame::Integer(0),
        Err(_) => Frame::wrongtype(),
    }
}

pub fn hexists(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() != 3 {
        return wrong_args("hexists");
    }
    match db.get_hash(&args[1]) {
        Ok(Some(h)) => Frame::Integer(h.contains_key(&args[2]) as i64),
        Ok(None) => Frame::Integer(0),
        Err(_) => Frame::wrongtype(),
    }
}

pub fn hstrlen(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() != 3 {
        return wrong_args("hstrlen");
    }
    match db.get_hash(&args[1]) {
        Ok(Some(h)) => Frame::Integer(h.get(&args[2]).map_or(0, |v| v.len()) as i64),
        Ok(None) => Frame::Integer(0),
        Err(_) => Frame::wrongtype(),
    }
}

pub fn hincrby(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() != 4 {
        return wrong_args("hincrby");
    }
    let incr = match parse_int(&args[3]) {
        Ok(n) => n,
        Err(e) => return e,
    };
    let h = match db.get_or_create_hash(args[1].clone()) {
        Ok(h) => h,
        Err(_) => return Frame::wrongtype(),
    };
    let cur = match h.get(&args[2]) {
        Some(v) => match std::str::from_utf8(v)
            .ok()
            .and_then(|s| s.parse::<i64>().ok())
        {
            Some(n) => n,
            None => return Frame::err("hash value is not an integer"),
        },
        None => 0,
    };
    match cur.checked_add(incr) {
        Some(next) => {
            h.insert(args[2].clone(), Bytes::from(next.to_string()));
            Frame::Integer(next)
        }
        None => Frame::err("increment or decrement would overflow"),
    }
}

pub fn hincrbyfloat(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() != 4 {
        return wrong_args("hincrbyfloat");
    }
    let incr = match parse_float(&args[3]) {
        Ok(f) => f,
        Err(e) => return e,
    };
    let h = match db.get_or_create_hash(args[1].clone()) {
        Ok(h) => h,
        Err(_) => return Frame::wrongtype(),
    };
    let cur = match h.get(&args[2]) {
        Some(v) => match std::str::from_utf8(v)
            .ok()
            .and_then(|s| s.trim().parse::<f64>().ok())
        {
            Some(f) => f,
            None => return Frame::err("hash value is not a float"),
        },
        None => 0.0,
    };
    let next = cur + incr;
    if next.is_nan() || next.is_infinite() {
        return Frame::err("increment would produce NaN or Infinity");
    }
    let s = format_double(next);
    h.insert(args[2].clone(), Bytes::from(s.clone()));
    Frame::Bulk(Bytes::from(s))
}

pub fn hscan(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() < 3 {
        return wrong_args("hscan");
    }
    let mut pattern: Option<Bytes> = None;
    let mut novalues = false;
    let mut i = 3;
    while i < args.len() {
        match upper(&args[i]).as_str() {
            "MATCH" if i + 1 < args.len() => {
                pattern = Some(args[i + 1].clone());
                i += 2;
            }
            "COUNT" if i + 1 < args.len() => i += 2,
            "NOVALUES" => {
                novalues = true;
                i += 1;
            }
            _ => return Frame::err("syntax error"),
        }
    }
    let items = match db.get_hash(&args[1]) {
        Ok(Some(h)) => {
            let mut out = Vec::new();
            for (k, v) in h {
                if pattern
                    .as_deref()
                    .map_or(true, |p| crate::db::glob_match(p, k))
                {
                    out.push(Frame::Bulk(k.clone()));
                    if !novalues {
                        out.push(Frame::Bulk(v.clone()));
                    }
                }
            }
            out
        }
        Ok(None) => vec![],
        Err(_) => return Frame::wrongtype(),
    };
    Frame::Array(vec![Frame::bulk("0"), Frame::Array(items)])
}

pub fn hrandfield(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() < 2 || args.len() > 4 {
        return wrong_args("hrandfield");
    }
    let entries: Vec<(Bytes, Bytes)> = match db.get_hash(&args[1]) {
        Ok(Some(h)) => h.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        Ok(None) => vec![],
        Err(_) => return Frame::wrongtype(),
    };

    // No count: return a single random field (or nil).
    if args.len() == 2 {
        if entries.is_empty() {
            return Frame::Null;
        }
        let idx = (rand_u64() % entries.len() as u64) as usize;
        return Frame::Bulk(entries[idx].0.clone());
    }

    let count = match parse_int(&args[2]) {
        Ok(n) => n,
        Err(e) => return e,
    };
    let withvalues = args.len() == 4 && upper(&args[3]) == "WITHVALUES";
    if args.len() == 4 && !withvalues {
        return Frame::err("syntax error");
    }

    let picked = pick(&entries, count);
    if withvalues {
        // RESP3 nests these as [field, value] pairs; RESP2 flattens them.
        Frame::Pairs(
            picked
                .into_iter()
                .map(|(k, v)| (Frame::Bulk(k), Frame::Bulk(v)))
                .collect(),
        )
    } else {
        Frame::Array(picked.into_iter().map(|(k, _)| Frame::Bulk(k)).collect())
    }
}

/// Pick `count` entries. Positive count: distinct, capped at the set size.
/// Negative count: `|count|` picks that may repeat.
fn pick(entries: &[(Bytes, Bytes)], count: i64) -> Vec<(Bytes, Bytes)> {
    if entries.is_empty() || count == 0 {
        return vec![];
    }
    if count < 0 {
        let n = (-count) as usize;
        (0..n)
            .map(|_| entries[(rand_u64() % entries.len() as u64) as usize].clone())
            .collect()
    } else {
        let n = (count as usize).min(entries.len());
        let mut idx: Vec<usize> = (0..entries.len()).collect();
        // Partial Fisher-Yates for the first n slots.
        for i in 0..n {
            let j = i + (rand_u64() as usize) % (entries.len() - i);
            idx.swap(i, j);
        }
        idx[..n].iter().map(|&i| entries[i].clone()).collect()
    }
}
