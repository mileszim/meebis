//! List commands, backed by a `VecDeque`.

use super::{norm_index, parse_int, upper, wrong_args};
use crate::db::Db;
use crate::resp::Frame;
use bytes::Bytes;

pub fn push(db: &mut Db, args: &[Bytes], left: bool, xx: bool) -> Frame {
    if args.len() < 3 {
        return wrong_args("lpush");
    }
    // The X variants do nothing (and never create) when the key is absent.
    if xx {
        match db.get_list(&args[1]) {
            Ok(Some(_)) => {}
            Ok(None) => return Frame::Integer(0),
            Err(_) => return Frame::wrongtype(),
        }
    }
    let list = match db.get_or_create_list(args[1].clone()) {
        Ok(l) => l,
        Err(_) => return Frame::wrongtype(),
    };
    for v in &args[2..] {
        if left {
            list.push_front(v.clone());
        } else {
            list.push_back(v.clone());
        }
    }
    Frame::Integer(list.len() as i64)
}

pub fn pop(db: &mut Db, args: &[Bytes], left: bool) -> Frame {
    if args.len() < 2 || args.len() > 3 {
        return wrong_args("lpop");
    }
    let count = match args.get(2) {
        Some(a) => match parse_int(a) {
            Ok(n) if n >= 0 => Some(n as usize),
            Ok(_) => return Frame::err("value is out of range, must be positive"),
            Err(e) => return e,
        },
        None => None,
    };

    let list = match db.get_list_mut(&args[1]) {
        Ok(Some(l)) => l,
        Ok(None) => {
            return if count.is_some() {
                Frame::NullArray
            } else {
                Frame::Null
            }
        }
        Err(_) => return Frame::wrongtype(),
    };

    let result = match count {
        None => match if left {
            list.pop_front()
        } else {
            list.pop_back()
        } {
            Some(v) => Frame::Bulk(v),
            None => Frame::Null,
        },
        Some(n) => {
            let take = n.min(list.len());
            let mut out = Vec::with_capacity(take);
            for _ in 0..take {
                let v = if left {
                    list.pop_front()
                } else {
                    list.pop_back()
                };
                if let Some(v) = v {
                    out.push(Frame::Bulk(v));
                }
            }
            Frame::Array(out)
        }
    };
    db.remove_if_empty(&args[1]);
    result
}

pub fn llen(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() != 2 {
        return wrong_args("llen");
    }
    match db.get_list(&args[1]) {
        Ok(Some(l)) => Frame::Integer(l.len() as i64),
        Ok(None) => Frame::Integer(0),
        Err(_) => Frame::wrongtype(),
    }
}

pub fn lrange(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() != 4 {
        return wrong_args("lrange");
    }
    let (start, stop) = match (parse_int(&args[2]), parse_int(&args[3])) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => return e,
    };
    let list = match db.get_list(&args[1]) {
        Ok(Some(l)) => l,
        Ok(None) => return Frame::Array(vec![]),
        Err(_) => return Frame::wrongtype(),
    };
    let len = list.len() as i64;
    let mut start = norm_index(start, list.len()).max(0);
    let mut stop = norm_index(stop, list.len());
    if stop >= len {
        stop = len - 1;
    }
    if start > stop || start >= len {
        return Frame::Array(vec![]);
    }
    if start < 0 {
        start = 0;
    }
    let out = (start..=stop)
        .filter_map(|i| list.get(i as usize).cloned().map(Frame::Bulk))
        .collect();
    Frame::Array(out)
}

pub fn lindex(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() != 3 {
        return wrong_args("lindex");
    }
    let idx = match parse_int(&args[2]) {
        Ok(n) => n,
        Err(e) => return e,
    };
    match db.get_list(&args[1]) {
        Ok(Some(l)) => {
            let i = norm_index(idx, l.len());
            if i < 0 {
                Frame::Null
            } else {
                l.get(i as usize)
                    .cloned()
                    .map(Frame::Bulk)
                    .unwrap_or(Frame::Null)
            }
        }
        Ok(None) => Frame::Null,
        Err(_) => Frame::wrongtype(),
    }
}

pub fn lset(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() != 4 {
        return wrong_args("lset");
    }
    let idx = match parse_int(&args[2]) {
        Ok(n) => n,
        Err(e) => return e,
    };
    match db.get_list_mut(&args[1]) {
        Ok(Some(l)) => {
            let i = norm_index(idx, l.len());
            if i < 0 || i as usize >= l.len() {
                Frame::err("index out of range")
            } else {
                l[i as usize] = args[3].clone();
                Frame::ok()
            }
        }
        Ok(None) => Frame::err("no such key"),
        Err(_) => Frame::wrongtype(),
    }
}

pub fn lrem(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() != 4 {
        return wrong_args("lrem");
    }
    let count = match parse_int(&args[2]) {
        Ok(n) => n,
        Err(e) => return e,
    };
    let target = &args[3];
    let list = match db.get_list_mut(&args[1]) {
        Ok(Some(l)) => l,
        Ok(None) => return Frame::Integer(0),
        Err(_) => return Frame::wrongtype(),
    };

    let mut removed = 0i64;
    if count >= 0 {
        // Front-to-back; count 0 means "all".
        let limit = if count == 0 {
            usize::MAX
        } else {
            count as usize
        };
        let mut i = 0;
        while i < list.len() && removed < limit as i64 {
            if list[i] == *target {
                list.remove(i);
                removed += 1;
            } else {
                i += 1;
            }
        }
    } else {
        // Back-to-front.
        let limit = (-count) as usize;
        let mut i = list.len();
        while i > 0 && (removed as usize) < limit {
            i -= 1;
            if list[i] == *target {
                list.remove(i);
                removed += 1;
            }
        }
    }
    db.remove_if_empty(&args[1]);
    Frame::Integer(removed)
}

pub fn ltrim(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() != 4 {
        return wrong_args("ltrim");
    }
    let (start, stop) = match (parse_int(&args[2]), parse_int(&args[3])) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => return e,
    };
    let list = match db.get_list_mut(&args[1]) {
        Ok(Some(l)) => l,
        Ok(None) => return Frame::ok(),
        Err(_) => return Frame::wrongtype(),
    };
    let len = list.len() as i64;
    let mut start = norm_index(start, list.len()).max(0);
    let mut stop = norm_index(stop, list.len());
    if start < 0 {
        start = 0;
    }
    if stop >= len {
        stop = len - 1;
    }
    if start > stop || start >= len {
        list.clear();
    } else {
        list.truncate(stop as usize + 1);
        list.drain(0..start as usize);
    }
    db.remove_if_empty(&args[1]);
    Frame::ok()
}

pub fn linsert(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() != 5 {
        return wrong_args("linsert");
    }
    let before = match upper(&args[2]).as_str() {
        "BEFORE" => true,
        "AFTER" => false,
        _ => return Frame::err("syntax error"),
    };
    let list = match db.get_list_mut(&args[1]) {
        Ok(Some(l)) => l,
        Ok(None) => return Frame::Integer(0),
        Err(_) => return Frame::wrongtype(),
    };
    match list.iter().position(|e| e == &args[3]) {
        Some(pos) => {
            let at = if before { pos } else { pos + 1 };
            list.insert(at, args[4].clone());
            Frame::Integer(list.len() as i64)
        }
        None => Frame::Integer(-1),
    }
}

pub fn lpos(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() < 3 {
        return wrong_args("lpos");
    }
    let target = &args[2];
    let mut rank: i64 = 1;
    let mut count: Option<i64> = None;
    let mut maxlen: usize = 0;
    let mut i = 3;
    while i < args.len() {
        match upper(&args[i]).as_str() {
            "RANK" if i + 1 < args.len() => {
                rank = match parse_int(&args[i + 1]) {
                    Ok(0) => return Frame::err("RANK can't be zero"),
                    Ok(n) => n,
                    Err(e) => return e,
                };
                i += 2;
            }
            "COUNT" if i + 1 < args.len() => {
                count = match parse_int(&args[i + 1]) {
                    Ok(n) if n >= 0 => Some(n),
                    Ok(_) => return Frame::err("COUNT can't be negative"),
                    Err(e) => return e,
                };
                i += 2;
            }
            "MAXLEN" if i + 1 < args.len() => {
                maxlen = match parse_int(&args[i + 1]) {
                    Ok(n) if n >= 0 => n as usize,
                    Ok(_) => return Frame::err("MAXLEN can't be negative"),
                    Err(e) => return e,
                };
                i += 2;
            }
            _ => return Frame::err("syntax error"),
        }
    }

    let list = match db.get_list(&args[1]) {
        Ok(Some(l)) => l,
        Ok(None) => {
            return if count.is_some() {
                Frame::Array(vec![])
            } else {
                Frame::Null
            }
        }
        Err(_) => return Frame::wrongtype(),
    };

    let len = list.len();
    let mut skip = rank.unsigned_abs() as usize - 1;
    let want = count.map(|c| if c == 0 { usize::MAX } else { c as usize });
    let mut found = Vec::new();
    let mut compared = 0usize;

    // Iterate in the direction implied by RANK's sign.
    let order: Box<dyn Iterator<Item = usize>> = if rank > 0 {
        Box::new(0..len)
    } else {
        Box::new((0..len).rev())
    };
    for idx in order {
        if maxlen != 0 && compared >= maxlen {
            break;
        }
        compared += 1;
        if list[idx] == *target {
            if skip > 0 {
                skip -= 1;
                continue;
            }
            found.push(idx as i64);
            if let Some(w) = want {
                if found.len() >= w {
                    break;
                }
            } else {
                break;
            }
        }
    }

    match count {
        Some(_) => Frame::Array(found.into_iter().map(Frame::Integer).collect()),
        None => found
            .first()
            .map(|&i| Frame::Integer(i))
            .unwrap_or(Frame::Null),
    }
}

pub fn rpoplpush(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() != 3 {
        return wrong_args("rpoplpush");
    }
    move_elem(db, &args[1], args[2].clone(), false, true)
}

pub fn lmove(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() != 5 {
        return wrong_args("lmove");
    }
    let from_left = match upper(&args[3]).as_str() {
        "LEFT" => true,
        "RIGHT" => false,
        _ => return Frame::err("syntax error"),
    };
    let to_left = match upper(&args[4]).as_str() {
        "LEFT" => true,
        "RIGHT" => false,
        _ => return Frame::err("syntax error"),
    };
    move_elem(db, &args[1], args[2].clone(), from_left, to_left)
}

/// Shared engine for RPOPLPUSH/LMOVE: pop one element off `src` and push it
/// onto `dst`. Both keys are type-checked before any mutation.
fn move_elem(db: &mut Db, src: &Bytes, dst: Bytes, from_left: bool, to_left: bool) -> Frame {
    // Validate dst is a list (or absent) before touching src.
    if db.get_list(&dst).is_err() {
        return Frame::wrongtype();
    }
    let elem = match db.get_list_mut(src) {
        Ok(Some(l)) => {
            if from_left {
                l.pop_front()
            } else {
                l.pop_back()
            }
        }
        Ok(None) => None,
        Err(_) => return Frame::wrongtype(),
    };
    let elem = match elem {
        Some(e) => e,
        None => return Frame::Null,
    };
    db.remove_if_empty(src);
    let d = match db.get_or_create_list(dst) {
        Ok(d) => d,
        Err(_) => return Frame::wrongtype(),
    };
    if to_left {
        d.push_front(elem.clone());
    } else {
        d.push_back(elem.clone());
    }
    Frame::Bulk(elem)
}
