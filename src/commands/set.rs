//! Set commands, backed by a `HashSet`.

use super::{parse_int, rand_u64, upper, wrong_args};
use crate::db::{Db, Value};
use crate::resp::Frame;
use bytes::Bytes;
use std::collections::HashSet;

pub fn sadd(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() < 3 {
        return wrong_args("sadd");
    }
    let set = match db.get_or_create_set(args[1].clone()) {
        Ok(s) => s,
        Err(_) => return Frame::wrongtype(),
    };
    let added = args[2..]
        .iter()
        .filter(|m| set.insert((*m).clone()))
        .count();
    Frame::Integer(added as i64)
}

pub fn srem(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() < 3 {
        return wrong_args("srem");
    }
    match db.get_set(&args[1]) {
        Ok(Some(_)) => {}
        Ok(None) => return Frame::Integer(0),
        Err(_) => return Frame::wrongtype(),
    }
    let removed = match db.get_or_create_set(args[1].clone()) {
        Ok(s) => args[2..].iter().filter(|m| s.remove(*m)).count(),
        Err(_) => return Frame::wrongtype(),
    };
    db.remove_if_empty(&args[1]);
    Frame::Integer(removed as i64)
}

pub fn smembers(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() != 2 {
        return wrong_args("smembers");
    }
    match db.get_set(&args[1]) {
        Ok(Some(s)) => Frame::Set(s.iter().cloned().map(Frame::Bulk).collect()),
        Ok(None) => Frame::Set(vec![]),
        Err(_) => Frame::wrongtype(),
    }
}

pub fn sismember(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() != 3 {
        return wrong_args("sismember");
    }
    match db.get_set(&args[1]) {
        Ok(Some(s)) => Frame::Integer(s.contains(&args[2]) as i64),
        Ok(None) => Frame::Integer(0),
        Err(_) => Frame::wrongtype(),
    }
}

pub fn smismember(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() < 3 {
        return wrong_args("smismember");
    }
    match db.get_set(&args[1]) {
        Ok(Some(s)) => Frame::Array(
            args[2..]
                .iter()
                .map(|m| Frame::Integer(s.contains(m) as i64))
                .collect(),
        ),
        Ok(None) => Frame::Array(args[2..].iter().map(|_| Frame::Integer(0)).collect()),
        Err(_) => Frame::wrongtype(),
    }
}

pub fn scard(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() != 2 {
        return wrong_args("scard");
    }
    match db.get_set(&args[1]) {
        Ok(Some(s)) => Frame::Integer(s.len() as i64),
        Ok(None) => Frame::Integer(0),
        Err(_) => Frame::wrongtype(),
    }
}

pub fn spop(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() < 2 || args.len() > 3 {
        return wrong_args("spop");
    }
    let count = match args.get(2) {
        Some(a) => match parse_int(a) {
            Ok(n) if n >= 0 => Some(n as usize),
            Ok(_) => return Frame::err("value is out of range, must be positive"),
            Err(e) => return e,
        },
        None => None,
    };

    let members: Vec<Bytes> = match db.get_set(&args[1]) {
        Ok(Some(s)) => s.iter().cloned().collect(),
        Ok(None) => {
            return if count.is_some() {
                Frame::Set(vec![])
            } else {
                Frame::Null
            }
        }
        Err(_) => return Frame::wrongtype(),
    };

    let chosen = match count {
        None => {
            if members.is_empty() {
                return Frame::Null;
            }
            vec![members[(rand_u64() % members.len() as u64) as usize].clone()]
        }
        Some(n) => distinct_sample(&members, n),
    };

    // Remove the chosen members.
    if let Ok(set) = db.get_or_create_set(args[1].clone()) {
        for m in &chosen {
            set.remove(m);
        }
    }
    db.remove_if_empty(&args[1]);

    match count {
        None => Frame::Bulk(chosen.into_iter().next().unwrap()),
        Some(_) => Frame::Set(chosen.into_iter().map(Frame::Bulk).collect()),
    }
}

pub fn srandmember(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() < 2 || args.len() > 3 {
        return wrong_args("srandmember");
    }
    let members: Vec<Bytes> = match db.get_set(&args[1]) {
        Ok(Some(s)) => s.iter().cloned().collect(),
        Ok(None) => {
            return if args.len() == 3 {
                Frame::Array(vec![])
            } else {
                Frame::Null
            }
        }
        Err(_) => return Frame::wrongtype(),
    };

    match args.get(2) {
        None => {
            if members.is_empty() {
                Frame::Null
            } else {
                Frame::Bulk(members[(rand_u64() % members.len() as u64) as usize].clone())
            }
        }
        Some(a) => {
            let count = match parse_int(a) {
                Ok(n) => n,
                Err(e) => return e,
            };
            let picks = if count < 0 {
                // With repeats.
                let n = (-count) as usize;
                (0..n)
                    .map(|_| members[(rand_u64() % members.len().max(1) as u64) as usize].clone())
                    .filter(|_| !members.is_empty())
                    .collect()
            } else {
                distinct_sample(&members, count as usize)
            };
            Frame::Array(picks.into_iter().map(Frame::Bulk).collect())
        }
    }
}

pub fn smove(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() != 4 {
        return wrong_args("smove");
    }
    let (src, dst, member) = (&args[1], args[2].clone(), &args[3]);

    // Type-check destination before mutating anything.
    if db.get_set(&dst).is_err() {
        return Frame::wrongtype();
    }
    let present = match db.get_set(src) {
        Ok(Some(s)) => s.contains(member),
        Ok(None) => false,
        Err(_) => return Frame::wrongtype(),
    };
    if !present {
        return Frame::Integer(0);
    }
    if let Ok(s) = db.get_or_create_set(src.clone()) {
        s.remove(member);
    }
    db.remove_if_empty(src);
    if let Ok(d) = db.get_or_create_set(dst) {
        d.insert(member.clone());
    }
    Frame::Integer(1)
}

/// Which multi-set operation to perform.
pub enum Op {
    Union,
    Inter,
    Diff,
}

pub fn setop(db: &mut Db, args: &[Bytes], op: Op, store: bool) -> Frame {
    let min = if store { 3 } else { 2 };
    if args.len() < min {
        return wrong_args("sunion");
    }
    let (dest, keys): (Option<&Bytes>, &[Bytes]) = if store {
        (Some(&args[1]), &args[2..])
    } else {
        (None, &args[1..])
    };

    let mut sets: Vec<HashSet<Bytes>> = Vec::with_capacity(keys.len());
    for key in keys {
        match db.get_set(key) {
            Ok(Some(s)) => sets.push(s.clone()),
            Ok(None) => sets.push(HashSet::new()),
            Err(_) => return Frame::wrongtype(),
        }
    }

    let result = compute_setop(sets, op);

    match dest {
        Some(d) => {
            let n = result.len();
            if n == 0 {
                db.remove(d);
            } else {
                db.set(d.clone(), Value::Set(result));
            }
            Frame::Integer(n as i64)
        }
        None => Frame::Set(result.into_iter().map(Frame::Bulk).collect()),
    }
}

fn compute_setop(mut sets: Vec<HashSet<Bytes>>, op: Op) -> HashSet<Bytes> {
    match op {
        Op::Union => sets.into_iter().flatten().collect(),
        Op::Inter => {
            if sets.is_empty() {
                return HashSet::new();
            }
            let mut base = sets.swap_remove(0);
            base.retain(|m| sets.iter().all(|s| s.contains(m)));
            base
        }
        Op::Diff => {
            if sets.is_empty() {
                return HashSet::new();
            }
            let mut base = sets.swap_remove(0);
            base.retain(|m| !sets.iter().any(|s| s.contains(m)));
            base
        }
    }
}

pub fn sintercard(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() < 3 {
        return wrong_args("sintercard");
    }
    let numkeys = match parse_int(&args[1]) {
        Ok(n) if n > 0 => n as usize,
        Ok(_) => return Frame::err("numkeys should be greater than 0"),
        Err(_) => return Frame::err("numkeys should be greater than 0"),
    };
    if args.len() < 2 + numkeys {
        return Frame::err("Number of keys can't be greater than number of args");
    }
    let keys = &args[2..2 + numkeys];
    let mut limit = 0usize;
    let mut i = 2 + numkeys;
    while i < args.len() {
        match upper(&args[i]).as_str() {
            "LIMIT" if i + 1 < args.len() => {
                limit = match parse_int(&args[i + 1]) {
                    Ok(n) if n >= 0 => n as usize,
                    _ => return Frame::err("LIMIT can't be negative"),
                };
                i += 2;
            }
            _ => return Frame::err("syntax error"),
        }
    }

    let mut sets: Vec<HashSet<Bytes>> = Vec::with_capacity(keys.len());
    for key in keys {
        match db.get_set(key) {
            Ok(Some(s)) => sets.push(s.clone()),
            Ok(None) => return Frame::Integer(0),
            Err(_) => return Frame::wrongtype(),
        }
    }
    let inter = compute_setop(sets, Op::Inter);
    let n = if limit == 0 {
        inter.len()
    } else {
        inter.len().min(limit)
    };
    Frame::Integer(n as i64)
}

pub fn sscan(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() < 3 {
        return wrong_args("sscan");
    }
    let mut pattern: Option<Bytes> = None;
    let mut i = 3;
    while i < args.len() {
        match upper(&args[i]).as_str() {
            "MATCH" if i + 1 < args.len() => {
                pattern = Some(args[i + 1].clone());
                i += 2;
            }
            "COUNT" if i + 1 < args.len() => i += 2,
            _ => return Frame::err("syntax error"),
        }
    }
    let items = match db.get_set(&args[1]) {
        Ok(Some(s)) => s
            .iter()
            .filter(|m| {
                pattern
                    .as_deref()
                    .map_or(true, |p| crate::db::glob_match(p, m))
            })
            .cloned()
            .map(Frame::Bulk)
            .collect(),
        Ok(None) => vec![],
        Err(_) => return Frame::wrongtype(),
    };
    Frame::Array(vec![Frame::bulk("0"), Frame::Array(items)])
}

/// Sample up to `n` distinct members via a partial Fisher-Yates shuffle.
fn distinct_sample(members: &[Bytes], n: usize) -> Vec<Bytes> {
    let n = n.min(members.len());
    let mut idx: Vec<usize> = (0..members.len()).collect();
    for i in 0..n {
        let j = i + (rand_u64() as usize) % (members.len() - i);
        idx.swap(i, j);
    }
    idx[..n].iter().map(|&i| members[i].clone()).collect()
}
