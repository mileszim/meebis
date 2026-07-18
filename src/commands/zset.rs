//! Sorted-set commands.
//!
//! Range and rank operations materialize the set into an ascending vector and
//! slice it. That is O(n) per call rather than O(log n), which is a deliberate
//! trade for simplicity at dev-tool scale.

use super::{parse_float, parse_int, rand_u64, upper, wrong_args};
use crate::db::{Db, ZSet};
use crate::resp::{format_double, Frame};
use bytes::Bytes;

pub fn zadd(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() < 4 {
        return wrong_args("zadd");
    }
    let mut nx = false;
    let mut xx = false;
    let mut gt = false;
    let mut lt = false;
    let mut ch = false;
    let mut incr = false;
    let mut i = 2;
    while i < args.len() {
        match upper(&args[i]).as_str() {
            "NX" => nx = true,
            "XX" => xx = true,
            "GT" => gt = true,
            "LT" => lt = true,
            "CH" => ch = true,
            "INCR" => incr = true,
            _ => break,
        }
        i += 1;
    }

    if nx && (xx || gt || lt) {
        return Frame::err("GT, LT, and/or NX options at the same time are not compatible");
    }
    if gt && lt {
        return Frame::err("GT, LT, and/or NX options at the same time are not compatible");
    }

    let rest = &args[i..];
    if rest.is_empty() || rest.len() % 2 != 0 {
        return Frame::err("syntax error");
    }
    if incr && rest.len() != 2 {
        return Frame::err("INCR option supports a single increment-element pair");
    }

    // Parse all scores up front so a bad score doesn't leave a partial write.
    let mut pairs: Vec<(f64, &Bytes)> = Vec::with_capacity(rest.len() / 2);
    let mut j = 0;
    while j < rest.len() {
        match parse_float(&rest[j]) {
            Ok(s) => pairs.push((s, &rest[j + 1])),
            Err(e) => return e,
        }
        j += 2;
    }

    let z = match db.get_or_create_zset(args[1].clone()) {
        Ok(z) => z,
        Err(_) => return Frame::wrongtype(),
    };

    let mut added = 0i64;
    let mut changed = 0i64;
    let mut incr_result: Option<f64> = None;

    for (score, member) in pairs {
        let current = z.score(member);
        let exists = current.is_some();
        if (nx && exists) || (xx && !exists) {
            if incr {
                incr_result = None;
            }
            continue;
        }
        let new_score = if incr {
            current.unwrap_or(0.0) + score
        } else {
            score
        };
        if new_score.is_nan() {
            db.remove_if_empty(&args[1]);
            return Frame::err("resulting score is not a number (NaN)");
        }
        if exists && (gt || lt) {
            let cur = current.unwrap();
            if (gt && new_score <= cur) || (lt && new_score >= cur) {
                if incr {
                    incr_result = None;
                }
                continue;
            }
        }
        let is_new = z.insert(member.clone(), new_score);
        if is_new {
            added += 1;
            changed += 1;
        } else if current != Some(new_score) {
            changed += 1;
        }
        if incr {
            incr_result = Some(new_score);
        }
    }

    db.remove_if_empty(&args[1]);

    if incr {
        match incr_result {
            Some(s) => Frame::Double(s),
            None => Frame::Null,
        }
    } else if ch {
        Frame::Integer(changed)
    } else {
        Frame::Integer(added)
    }
}

pub fn zincrby(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() != 4 {
        return wrong_args("zincrby");
    }
    let incr = match parse_float(&args[2]) {
        Ok(f) => f,
        Err(e) => return e,
    };
    let z = match db.get_or_create_zset(args[1].clone()) {
        Ok(z) => z,
        Err(_) => return Frame::wrongtype(),
    };
    let new = z.score(&args[3]).unwrap_or(0.0) + incr;
    if new.is_nan() {
        db.remove_if_empty(&args[1]);
        return Frame::err("resulting score is not a number (NaN)");
    }
    z.insert(args[3].clone(), new);
    Frame::Double(new)
}

pub fn zrem(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() < 3 {
        return wrong_args("zrem");
    }
    let z = match db.get_or_create_zset(args[1].clone()) {
        Ok(z) => z,
        Err(_) => return Frame::wrongtype(),
    };
    let removed = args[2..].iter().filter(|m| z.remove(m)).count();
    db.remove_if_empty(&args[1]);
    Frame::Integer(removed as i64)
}

pub fn zscore(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() != 3 {
        return wrong_args("zscore");
    }
    match db.get_zset(&args[1]) {
        Ok(Some(z)) => match z.score(&args[2]) {
            Some(s) => Frame::Double(s),
            None => Frame::Null,
        },
        Ok(None) => Frame::Null,
        Err(_) => Frame::wrongtype(),
    }
}

pub fn zmscore(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() < 3 {
        return wrong_args("zmscore");
    }
    match db.get_zset(&args[1]) {
        Ok(Some(z)) => Frame::Array(
            args[2..]
                .iter()
                .map(|m| match z.score(m) {
                    Some(s) => Frame::Double(s),
                    None => Frame::Null,
                })
                .collect(),
        ),
        Ok(None) => Frame::Array(args[2..].iter().map(|_| Frame::Null).collect()),
        Err(_) => Frame::wrongtype(),
    }
}

pub fn zcard(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() != 2 {
        return wrong_args("zcard");
    }
    match db.get_zset(&args[1]) {
        Ok(Some(z)) => Frame::Integer(z.len() as i64),
        Ok(None) => Frame::Integer(0),
        Err(_) => Frame::wrongtype(),
    }
}

pub fn zrank(db: &mut Db, args: &[Bytes], rev: bool) -> Frame {
    if args.len() < 3 || args.len() > 4 {
        return wrong_args("zrank");
    }
    let withscore = args.len() == 4 && upper(&args[3]) == "WITHSCORE";
    if args.len() == 4 && !withscore {
        return Frame::err("syntax error");
    }
    match db.get_zset(&args[1]) {
        Ok(Some(z)) => match z.rank(&args[2]) {
            Some(r) => {
                let rank = if rev { z.len() - 1 - r } else { r } as i64;
                if withscore {
                    Frame::Array(vec![
                        Frame::Integer(rank),
                        Frame::Double(z.score(&args[2]).unwrap()),
                    ])
                } else {
                    Frame::Integer(rank)
                }
            }
            None => {
                if withscore {
                    Frame::NullArray
                } else {
                    Frame::Null
                }
            }
        },
        Ok(None) => {
            if withscore {
                Frame::NullArray
            } else {
                Frame::Null
            }
        }
        Err(_) => Frame::wrongtype(),
    }
}

pub fn zcount(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() != 4 {
        return wrong_args("zcount");
    }
    let min = match parse_score_bound(&args[2]) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let max = match parse_score_bound(&args[3]) {
        Ok(b) => b,
        Err(e) => return e,
    };
    match db.get_zset(&args[1]) {
        Ok(Some(z)) => {
            let n = z
                .iter_asc()
                .filter(|(_, s)| min.contains(*s) && max.contains_max(*s))
                .count();
            Frame::Integer(n as i64)
        }
        Ok(None) => Frame::Integer(0),
        Err(_) => Frame::wrongtype(),
    }
}

pub fn zrange(db: &mut Db, args: &[Bytes], rev_cmd: bool) -> Frame {
    if args.len() < 4 {
        return wrong_args("zrange");
    }
    let mut byscore = false;
    let mut bylex = false;
    let mut rev = rev_cmd;
    let mut withscores = false;
    let mut limit: Option<(i64, i64)> = None;
    let mut i = 4;
    while i < args.len() {
        match upper(&args[i]).as_str() {
            "BYSCORE" => byscore = true,
            "BYLEX" => bylex = true,
            "REV" => rev = true,
            "WITHSCORES" => withscores = true,
            "LIMIT" if i + 2 < args.len() => {
                let off = match parse_int(&args[i + 1]) {
                    Ok(n) => n,
                    Err(e) => return e,
                };
                let cnt = match parse_int(&args[i + 2]) {
                    Ok(n) => n,
                    Err(e) => return e,
                };
                limit = Some((off, cnt));
                i += 2;
            }
            _ => return Frame::err("syntax error"),
        }
        i += 1;
    }
    if limit.is_some() && !byscore && !bylex {
        return Frame::err(
            "syntax error, LIMIT is only supported in combination with either BYSCORE or BYLEX",
        );
    }
    if withscores && bylex {
        return Frame::err("syntax error, WITHSCORES not supported in combination with BYLEX");
    }

    let items = match db.get_zset(&args[1]) {
        Ok(Some(z)) => materialize(z),
        Ok(None) => return Frame::Array(vec![]),
        Err(_) => return Frame::wrongtype(),
    };

    if byscore {
        range_by_score(&items, &args[2], &args[3], rev, limit, withscores)
    } else if bylex {
        range_by_lex(&items, &args[2], &args[3], rev, limit)
    } else {
        range_by_index(&items, &args[2], &args[3], rev, withscores)
    }
}

pub fn zrangebyscore(db: &mut Db, args: &[Bytes], rev: bool) -> Frame {
    if args.len() < 4 {
        return wrong_args("zrangebyscore");
    }
    let mut withscores = false;
    let mut limit: Option<(i64, i64)> = None;
    let mut i = 4;
    while i < args.len() {
        match upper(&args[i]).as_str() {
            "WITHSCORES" => withscores = true,
            "LIMIT" if i + 2 < args.len() => {
                let off = match parse_int(&args[i + 1]) {
                    Ok(n) => n,
                    Err(e) => return e,
                };
                let cnt = match parse_int(&args[i + 2]) {
                    Ok(n) => n,
                    Err(e) => return e,
                };
                limit = Some((off, cnt));
                i += 2;
            }
            _ => return Frame::err("syntax error"),
        }
        i += 1;
    }
    let items = match db.get_zset(&args[1]) {
        Ok(Some(z)) => materialize(z),
        Ok(None) => return Frame::Array(vec![]),
        Err(_) => return Frame::wrongtype(),
    };
    // ZREVRANGEBYSCORE takes its bounds as (max, min).
    let (lo, hi) = if rev {
        (&args[3], &args[2])
    } else {
        (&args[2], &args[3])
    };
    range_by_score(&items, lo, hi, rev, limit, withscores)
}

pub fn zrangebylex(db: &mut Db, args: &[Bytes], rev: bool) -> Frame {
    if args.len() < 4 {
        return wrong_args("zrangebylex");
    }
    let mut limit: Option<(i64, i64)> = None;
    let mut i = 4;
    while i < args.len() {
        match upper(&args[i]).as_str() {
            "LIMIT" if i + 2 < args.len() => {
                let off = match parse_int(&args[i + 1]) {
                    Ok(n) => n,
                    Err(e) => return e,
                };
                let cnt = match parse_int(&args[i + 2]) {
                    Ok(n) => n,
                    Err(e) => return e,
                };
                limit = Some((off, cnt));
                i += 2;
            }
            _ => return Frame::err("syntax error"),
        }
        i += 1;
    }
    let items = match db.get_zset(&args[1]) {
        Ok(Some(z)) => materialize(z),
        Ok(None) => return Frame::Array(vec![]),
        Err(_) => return Frame::wrongtype(),
    };
    // range_by_lex swaps its bounds when rev is set, so pass them positionally.
    range_by_lex(&items, &args[2], &args[3], rev, limit)
}

pub fn zlexcount(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() != 4 {
        return wrong_args("zlexcount");
    }
    let min = match parse_lex_bound(&args[2]) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let max = match parse_lex_bound(&args[3]) {
        Ok(b) => b,
        Err(e) => return e,
    };
    match db.get_zset(&args[1]) {
        Ok(Some(z)) => {
            let n = z
                .iter_asc()
                .filter(|&(m, _)| min.ge_ok(m) && max.le_ok(m))
                .count();
            Frame::Integer(n as i64)
        }
        Ok(None) => Frame::Integer(0),
        Err(_) => Frame::wrongtype(),
    }
}

pub fn zpop(db: &mut Db, args: &[Bytes], min: bool) -> Frame {
    if args.len() < 2 || args.len() > 3 {
        return wrong_args("zpopmin");
    }
    // Redis distinguishes the no-count form (a flat [member, score] reply)
    // from the count form (an array of [member, score] pairs in RESP3).
    let has_count = args.len() == 3;
    let count = if has_count {
        match parse_int(&args[2]) {
            Ok(n) if n >= 0 => n as usize,
            Ok(_) => return Frame::err("value is out of range, must be positive"),
            Err(e) => return e,
        }
    } else {
        1
    };
    let popped: Vec<(Bytes, f64)> = match db.get_zset(&args[1]) {
        Ok(Some(z)) => {
            let items = materialize(z);
            if min {
                items.into_iter().take(count).collect()
            } else {
                items.into_iter().rev().take(count).collect()
            }
        }
        Ok(None) => return Frame::Array(vec![]),
        Err(_) => return Frame::wrongtype(),
    };
    if let Ok(z) = db.get_or_create_zset(args[1].clone()) {
        for (m, _) in &popped {
            z.remove(m);
        }
    }
    db.remove_if_empty(&args[1]);
    if has_count {
        emit(popped, true)
    } else {
        match popped.into_iter().next() {
            Some((m, s)) => Frame::Array(vec![Frame::Bulk(m), Frame::Double(s)]),
            None => Frame::Array(vec![]),
        }
    }
}

pub fn zremrangebyrank(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() != 4 {
        return wrong_args("zremrangebyrank");
    }
    let (start, stop) = match (parse_int(&args[2]), parse_int(&args[3])) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => return e,
    };
    let items = match db.get_zset(&args[1]) {
        Ok(Some(z)) => materialize(z),
        Ok(None) => return Frame::Integer(0),
        Err(_) => return Frame::wrongtype(),
    };
    let len = items.len() as i64;
    let (s, e) = clamp_range(start, stop, len);
    if s > e {
        return Frame::Integer(0);
    }
    let victims: Vec<Bytes> = items[s as usize..=e as usize]
        .iter()
        .map(|(m, _)| m.clone())
        .collect();
    let n = victims.len();
    if let Ok(z) = db.get_or_create_zset(args[1].clone()) {
        for m in &victims {
            z.remove(m);
        }
    }
    db.remove_if_empty(&args[1]);
    Frame::Integer(n as i64)
}

pub fn zremrangebyscore(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() != 4 {
        return wrong_args("zremrangebyscore");
    }
    let min = match parse_score_bound(&args[2]) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let max = match parse_score_bound(&args[3]) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let items = match db.get_zset(&args[1]) {
        Ok(Some(z)) => materialize(z),
        Ok(None) => return Frame::Integer(0),
        Err(_) => return Frame::wrongtype(),
    };
    let victims: Vec<Bytes> = items
        .into_iter()
        .filter(|(_, s)| min.contains(*s) && max.contains_max(*s))
        .map(|(m, _)| m)
        .collect();
    let n = victims.len();
    if let Ok(z) = db.get_or_create_zset(args[1].clone()) {
        for m in &victims {
            z.remove(m);
        }
    }
    db.remove_if_empty(&args[1]);
    Frame::Integer(n as i64)
}

pub fn zscan(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() < 3 {
        return wrong_args("zscan");
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
    let items = match db.get_zset(&args[1]) {
        Ok(Some(z)) => {
            let mut out = Vec::new();
            for (m, s) in z.iter_asc() {
                if pattern
                    .as_deref()
                    .map_or(true, |p| crate::db::glob_match(p, m))
                {
                    out.push(Frame::Bulk(m.clone()));
                    out.push(Frame::bulk(format_double(s)));
                }
            }
            out
        }
        Ok(None) => vec![],
        Err(_) => return Frame::wrongtype(),
    };
    Frame::Array(vec![Frame::bulk("0"), Frame::Array(items)])
}

pub fn zrandmember(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() < 2 || args.len() > 4 {
        return wrong_args("zrandmember");
    }
    let members = match db.get_zset(&args[1]) {
        Ok(Some(z)) => materialize(z),
        Ok(None) => vec![],
        Err(_) => return Frame::wrongtype(),
    };

    // No count: a single random member (or nil).
    if args.len() == 2 {
        if members.is_empty() {
            return Frame::Null;
        }
        let idx = (rand_u64() % members.len() as u64) as usize;
        return Frame::Bulk(members[idx].0.clone());
    }

    let count = match parse_int(&args[2]) {
        Ok(n) => n,
        Err(e) => return e,
    };
    let withscores = args.len() == 4 && upper(&args[3]) == "WITHSCORES";
    if args.len() == 4 && !withscores {
        return Frame::err("syntax error");
    }

    let picked = sample(&members, count);
    if withscores {
        Frame::Pairs(
            picked
                .into_iter()
                .map(|(m, s)| (Frame::Bulk(m), Frame::Double(s)))
                .collect(),
        )
    } else {
        Frame::Array(picked.into_iter().map(|(m, _)| Frame::Bulk(m)).collect())
    }
}

/// Sample entries: positive count is distinct (capped at the set size),
/// negative count allows repeats up to `|count|`.
fn sample(entries: &[(Bytes, f64)], count: i64) -> Vec<(Bytes, f64)> {
    if entries.is_empty() || count == 0 {
        return vec![];
    }
    if count < 0 {
        (0..(-count) as usize)
            .map(|_| entries[(rand_u64() % entries.len() as u64) as usize].clone())
            .collect()
    } else {
        let n = (count as usize).min(entries.len());
        let mut idx: Vec<usize> = (0..entries.len()).collect();
        for i in 0..n {
            let j = i + (rand_u64() as usize) % (entries.len() - i);
            idx.swap(i, j);
        }
        idx[..n].iter().map(|&i| entries[i].clone()).collect()
    }
}

// --- helpers ---

fn materialize(z: &ZSet) -> Vec<(Bytes, f64)> {
    z.iter_asc().map(|(m, s)| (m.clone(), s)).collect()
}

/// Turn a member/score list into a reply. With scores it becomes a
/// protocol-aware [`Frame::Pairs`]; without, a plain array of members.
fn emit(items: Vec<(Bytes, f64)>, withscores: bool) -> Frame {
    if withscores {
        Frame::Pairs(
            items
                .into_iter()
                .map(|(m, s)| (Frame::Bulk(m), Frame::Double(s)))
                .collect(),
        )
    } else {
        Frame::Array(items.into_iter().map(|(m, _)| Frame::Bulk(m)).collect())
    }
}

/// Normalize a possibly-negative inclusive rank range against `len`.
fn clamp_range(start: i64, stop: i64, len: i64) -> (i64, i64) {
    let mut s = if start < 0 { start + len } else { start };
    let mut e = if stop < 0 { stop + len } else { stop };
    if s < 0 {
        s = 0;
    }
    if e >= len {
        e = len - 1;
    }
    (s, e)
}

fn range_by_index(
    items: &[(Bytes, f64)],
    start: &Bytes,
    stop: &Bytes,
    rev: bool,
    withscores: bool,
) -> Frame {
    let start = match parse_int(start) {
        Ok(n) => n,
        Err(e) => return e,
    };
    let stop = match parse_int(stop) {
        Ok(n) => n,
        Err(e) => return e,
    };
    let ordered: Vec<(Bytes, f64)> = if rev {
        items.iter().rev().cloned().collect()
    } else {
        items.to_vec()
    };
    let len = ordered.len() as i64;
    let (s, e) = clamp_range(start, stop, len);
    if s > e || s >= len {
        return Frame::Array(vec![]);
    }
    emit(ordered[s as usize..=e as usize].to_vec(), withscores)
}

fn range_by_score(
    items: &[(Bytes, f64)],
    lo: &Bytes,
    hi: &Bytes,
    rev: bool,
    limit: Option<(i64, i64)>,
    withscores: bool,
) -> Frame {
    let min = match parse_score_bound(lo) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let max = match parse_score_bound(hi) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let mut selected: Vec<(Bytes, f64)> = items
        .iter()
        .filter(|(_, s)| min.contains(*s) && max.contains_max(*s))
        .cloned()
        .collect();
    if rev {
        selected.reverse();
    }
    let selected = apply_limit(selected, limit);
    emit(selected, withscores)
}

fn range_by_lex(
    items: &[(Bytes, f64)],
    lo: &Bytes,
    hi: &Bytes,
    rev: bool,
    limit: Option<(i64, i64)>,
) -> Frame {
    // For REV the client passes (max, min); swap so `min`/`max` are ordered.
    let (lo, hi) = if rev { (hi, lo) } else { (lo, hi) };
    let min = match parse_lex_bound(lo) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let max = match parse_lex_bound(hi) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let mut selected: Vec<(Bytes, f64)> = items
        .iter()
        .filter(|(m, _)| min.ge_ok(m) && max.le_ok(m))
        .cloned()
        .collect();
    if rev {
        selected.reverse();
    }
    let selected = apply_limit(selected, limit);
    emit(selected, false)
}

fn apply_limit(items: Vec<(Bytes, f64)>, limit: Option<(i64, i64)>) -> Vec<(Bytes, f64)> {
    match limit {
        None => items,
        Some((offset, count)) => {
            let offset = offset.max(0) as usize;
            let iter = items.into_iter().skip(offset);
            if count < 0 {
                iter.collect()
            } else {
                iter.take(count as usize).collect()
            }
        }
    }
}

/// A score-range endpoint: a value plus whether it is exclusive.
struct ScoreBound {
    value: f64,
    exclusive: bool,
}

impl ScoreBound {
    /// True if `score` satisfies this bound treated as a lower bound.
    fn contains(&self, score: f64) -> bool {
        if self.exclusive {
            score > self.value
        } else {
            score >= self.value
        }
    }
    /// True if `score` satisfies this bound treated as an upper bound.
    fn contains_max(&self, score: f64) -> bool {
        if self.exclusive {
            score < self.value
        } else {
            score <= self.value
        }
    }
}

fn parse_score_bound(b: &[u8]) -> Result<ScoreBound, Frame> {
    let s = std::str::from_utf8(b).map_err(|_| Frame::err("min or max is not a float"))?;
    let (exclusive, num) = match s.strip_prefix('(') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    let value = match num {
        "inf" | "+inf" | "infinity" | "+infinity" => f64::INFINITY,
        "-inf" | "-infinity" => f64::NEG_INFINITY,
        other => match other.trim().parse::<f64>() {
            Ok(f) if !f.is_nan() => f,
            _ => return Err(Frame::err("min or max is not a float")),
        },
    };
    Ok(ScoreBound { value, exclusive })
}

/// A lexicographic-range endpoint.
enum LexBound {
    NegInf,
    PosInf,
    Incl(Bytes),
    Excl(Bytes),
}

impl LexBound {
    /// Treated as a lower bound: is `m` >= this bound?
    fn ge_ok(&self, m: &Bytes) -> bool {
        match self {
            LexBound::NegInf => true,
            LexBound::PosInf => false,
            LexBound::Incl(v) => m.as_ref() >= v.as_ref(),
            LexBound::Excl(v) => m.as_ref() > v.as_ref(),
        }
    }
    /// Treated as an upper bound: is `m` <= this bound?
    fn le_ok(&self, m: &Bytes) -> bool {
        match self {
            LexBound::NegInf => false,
            LexBound::PosInf => true,
            LexBound::Incl(v) => m.as_ref() <= v.as_ref(),
            LexBound::Excl(v) => m.as_ref() < v.as_ref(),
        }
    }
}

fn parse_lex_bound(b: &[u8]) -> Result<LexBound, Frame> {
    match b.first() {
        Some(b'-') if b.len() == 1 => Ok(LexBound::NegInf),
        Some(b'+') if b.len() == 1 => Ok(LexBound::PosInf),
        Some(b'[') => Ok(LexBound::Incl(Bytes::copy_from_slice(&b[1..]))),
        Some(b'(') => Ok(LexBound::Excl(Bytes::copy_from_slice(&b[1..]))),
        _ => Err(Frame::err("not valid string range item")),
    }
}
