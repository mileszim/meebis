//! Redis Streams: `XADD`, `XLEN`, `XRANGE`, `XREVRANGE`, `XREAD`, `XDEL`,
//! `XTRIM`. Only what BullMQ actually exercises — no consumer groups.

use super::{parse_int, upper, wrong_args};
use crate::db::{now_ms, Db, Stream, StreamId};
use crate::resp::Frame;
use bytes::Bytes;
use std::str;

/// `XADD key [NOMKSTREAM] [MAXLEN|MINID [~|=] threshold [LIMIT c]] <id | *> field value ...`
pub fn xadd(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() < 5 {
        return wrong_args("xadd");
    }
    let key = &args[1];
    let mut i = 2;
    let mut nomkstream = false;
    let mut trim: Option<TrimSpec> = None;

    while i < args.len() {
        let up = upper(&args[i]);
        match up.as_str() {
            "NOMKSTREAM" => {
                nomkstream = true;
                i += 1;
            }
            "MAXLEN" | "MINID" => {
                let by_min_id = up == "MINID";
                i += 1;
                if i >= args.len() {
                    return Frame::err("syntax error");
                }
                // Optional ~ or = strategy modifier.
                if matches!(args[i].as_ref(), b"~" | b"=") {
                    i += 1;
                    if i >= args.len() {
                        return Frame::err("syntax error");
                    }
                }
                let threshold = args[i].clone();
                i += 1;
                let mut limit: Option<i64> = None;
                if i < args.len() && upper(&args[i]) == "LIMIT" {
                    i += 1;
                    if i >= args.len() {
                        return Frame::err("syntax error");
                    }
                    match parse_int(&args[i]) {
                        Ok(n) => limit = Some(n),
                        Err(e) => return e,
                    }
                    i += 1;
                }
                trim = Some(if by_min_id {
                    let id = match parse_id(&threshold, true) {
                        Ok(id) => id,
                        Err(e) => return e,
                    };
                    TrimSpec::MinId(id, limit)
                } else {
                    match parse_int(&threshold) {
                        Ok(n) if n >= 0 => TrimSpec::MaxLen(n as usize, limit),
                        Ok(_) => return Frame::err("MAXLEN can't be negative"),
                        Err(e) => return e,
                    }
                });
            }
            _ => break,
        }
    }

    if i >= args.len() {
        return wrong_args("xadd");
    }
    let id_arg = &args[i];
    i += 1;
    let field_start = i;
    if field_start >= args.len() || (args.len() - field_start) % 2 != 0 {
        return wrong_args("xadd");
    }

    // Auto-create unless NOMKSTREAM told us not to.
    if nomkstream {
        match db.get_stream(key) {
            Ok(None) => return Frame::Null,
            Err(_) => return Frame::wrongtype(),
            Ok(Some(_)) => {}
        }
    }

    let stream = match db.get_or_create_stream(key.clone()) {
        Ok(s) => s,
        Err(_) => return Frame::wrongtype(),
    };

    let id = match resolve_add_id(id_arg, stream) {
        Ok(id) => id,
        Err(e) => return e,
    };

    // XADD to an empty stream where the caller passes 0-0 is rejected.
    if id == StreamId::MIN {
        return Frame::err("The ID specified in XADD must be greater than 0-0");
    }

    let mut fields = Vec::with_capacity((args.len() - field_start) / 2);
    let mut j = field_start;
    while j < args.len() {
        fields.push((args[j].clone(), args[j + 1].clone()));
        j += 2;
    }

    stream.entries.insert(id, fields);
    stream.last_id = id;

    if let Some(spec) = trim {
        apply_trim(stream, spec);
    }

    Frame::Bulk(Bytes::from(id.to_string()))
}

/// `XLEN key`
pub fn xlen(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() != 2 {
        return wrong_args("xlen");
    }
    match db.get_stream(&args[1]) {
        Ok(None) => Frame::Integer(0),
        Ok(Some(s)) => Frame::Integer(s.entries.len() as i64),
        Err(_) => Frame::wrongtype(),
    }
}

/// `XRANGE key start end [COUNT c]` (`rev` swaps `start`/`end` and reverses).
pub fn xrange(db: &mut Db, args: &[Bytes], rev: bool) -> Frame {
    if args.len() < 4 {
        return wrong_args(if rev { "xrevrange" } else { "xrange" });
    }
    let (start_arg, end_arg) = if rev {
        (&args[3], &args[2])
    } else {
        (&args[2], &args[3])
    };
    let (start, start_exclusive) = match parse_range_bound(start_arg, false) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let (end, end_exclusive) = match parse_range_bound(end_arg, true) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let mut count: Option<usize> = None;
    let mut i = 4;
    while i < args.len() {
        if upper(&args[i]) == "COUNT" && i + 1 < args.len() {
            match parse_int(&args[i + 1]) {
                Ok(n) if n >= 0 => count = Some(n as usize),
                Ok(_) => return Frame::err("COUNT can't be negative"),
                Err(e) => return e,
            }
            i += 2;
        } else {
            return Frame::err("syntax error");
        }
    }

    let stream = match db.get_stream(&args[1]) {
        Ok(None) => return Frame::Array(vec![]),
        Ok(Some(s)) => s,
        Err(_) => return Frame::wrongtype(),
    };

    // Materialize once so we can iterate forward or reversed without a boxed
    // trait object (which triggers a very-complex-type clippy lint).
    let range: Vec<(StreamId, &Vec<(Bytes, Bytes)>)> = stream
        .entries
        .range(start..=end)
        .map(|(id, f)| (*id, f))
        .collect();
    let ordered: Vec<(StreamId, &Vec<(Bytes, Bytes)>)> = if rev {
        range.into_iter().rev().collect()
    } else {
        range
    };
    let mut items = Vec::new();
    for (id, fields) in ordered {
        if start_exclusive && id == start {
            continue;
        }
        if end_exclusive && id == end {
            continue;
        }
        items.push(entry_frame(id, fields));
        if let Some(n) = count {
            if items.len() >= n {
                break;
            }
        }
    }
    Frame::Array(items)
}

/// `XREAD [COUNT c] [BLOCK ms] STREAMS key [key ...] id [id ...]`. This is the
/// non-blocking form — [`read_block_spec`] extracts the BLOCK timeout for the
/// connection loop to enforce; this returns nil when no keys have new data.
pub fn xread(db: &mut Db, args: &[Bytes]) -> Frame {
    let parsed = match parse_xread(args) {
        Ok(p) => p,
        Err(e) => return e,
    };
    // RESP2 wants `[[stream, [entries]], ...]`; RESP3 wants a map keyed by
    // stream name. `Frame::Map` renders as an array-of-arrays in RESP2 and
    // as a `%` map in RESP3, but the RESP2 shape it uses is a flat 2n array —
    // XREAD needs `[[key, entries], ...]` instead. So encode by hand.
    let mut pairs: Vec<(Frame, Frame)> = Vec::new();
    for (key, from) in &parsed.pairs {
        let stream = match db.get_stream(key) {
            Ok(None) => continue,
            Ok(Some(s)) => s,
            Err(_) => return Frame::wrongtype(),
        };
        let lower_bound = from.next().unwrap_or(StreamId::MAX);
        let mut items = Vec::new();
        for (id, fields) in stream.entries.range(lower_bound..) {
            items.push(entry_frame(*id, fields));
            if let Some(n) = parsed.count {
                if items.len() >= n {
                    break;
                }
            }
        }
        if !items.is_empty() {
            pairs.push((Frame::Bulk(key.clone()), Frame::Array(items)));
        }
    }
    if pairs.is_empty() {
        Frame::NullArray
    } else {
        Frame::XReadReply(pairs)
    }
}

/// If args start with `[COUNT c] [BLOCK ms] STREAMS ...`, extract the BLOCK
/// timeout in milliseconds. `Some(0)` means block forever; `Some(n)` means
/// block up to `n` ms; `None` means non-blocking.
pub fn xread_block_ms(args: &[Bytes]) -> Option<u64> {
    // Skip the first arg (command name), then walk options.
    let mut i = 1;
    while i < args.len() {
        let up = upper(&args[i]);
        match up.as_str() {
            "COUNT" => i += 2,
            "BLOCK" => {
                if let Ok(n) = parse_int(args.get(i + 1)?) {
                    if n >= 0 {
                        return Some(n as u64);
                    }
                }
                return None;
            }
            _ => return None,
        }
    }
    None
}

/// Parsed XREAD/XREAD-once form: (key, from-id) pairs plus optional COUNT.
struct ParsedXread {
    pairs: Vec<(Bytes, StreamId)>,
    count: Option<usize>,
}

fn parse_xread(args: &[Bytes]) -> Result<ParsedXread, Frame> {
    let mut i = 1;
    let mut count: Option<usize> = None;
    let mut streams_idx: Option<usize> = None;
    while i < args.len() {
        let up = upper(&args[i]);
        match up.as_str() {
            "COUNT" => {
                let n = args.get(i + 1).ok_or_else(|| Frame::err("syntax error"))?;
                match parse_int(n) {
                    Ok(v) if v >= 0 => count = Some(v as usize),
                    Ok(_) => return Err(Frame::err("COUNT can't be negative")),
                    Err(e) => return Err(e),
                }
                i += 2;
            }
            "BLOCK" => {
                // Validated but ignored here; the loop honours it.
                if args.get(i + 1).is_none() {
                    return Err(Frame::err("syntax error"));
                }
                i += 2;
            }
            "STREAMS" => {
                streams_idx = Some(i + 1);
                break;
            }
            _ => return Err(Frame::err("syntax error")),
        }
    }
    let s =
        streams_idx.ok_or_else(|| Frame::err("syntax error, STREAMS option must be specified."))?;
    let rest = &args[s..];
    if rest.is_empty() || rest.len() % 2 != 0 {
        return Err(Frame::err(
            "Unbalanced XREAD list of streams: for each stream key an ID or '$' must be specified.",
        ));
    }
    let n = rest.len() / 2;
    let mut pairs = Vec::with_capacity(n);
    for k in 0..n {
        let key = rest[k].clone();
        let id_arg = &rest[n + k];
        let from = if id_arg.as_ref() == b"$" {
            // `$`: read only what's *newer than* whatever's currently last.
            // Resolve against the current state at parse time.
            StreamId::MAX
        } else {
            parse_id(id_arg, false)?
        };
        pairs.push((key, from));
    }
    Ok(ParsedXread { pairs, count })
}

/// Snapshot each `$` id to the stream's current `last_id` so the caller can
/// block for entries added *after* this snapshot.
pub fn resolve_dollar_ids(db: &mut Db, args: &[Bytes]) -> Result<Vec<Bytes>, Frame> {
    // Find STREAMS index, then split into keys | ids.
    let mut i = 1;
    while i < args.len() {
        let up = upper(&args[i]);
        if up == "STREAMS" {
            break;
        }
        if up == "COUNT" || up == "BLOCK" {
            i += 2;
            continue;
        }
        return Err(Frame::err("syntax error"));
    }
    let sidx = i + 1;
    if sidx >= args.len() {
        return Ok(args.to_vec());
    }
    let rest_len = args.len() - sidx;
    if rest_len % 2 != 0 {
        return Ok(args.to_vec());
    }
    let n = rest_len / 2;
    let mut new_args = args.to_vec();
    for k in 0..n {
        let idx = sidx + n + k;
        if args[idx].as_ref() == b"$" {
            let key = &args[sidx + k];
            let last = match db.get_stream(key) {
                Ok(Some(s)) => s.last_id,
                _ => StreamId::MIN,
            };
            new_args[idx] = Bytes::from(last.to_string());
        }
    }
    Ok(new_args)
}

/// `XDEL key id [id ...]`
pub fn xdel(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() < 3 {
        return wrong_args("xdel");
    }
    let mut ids = Vec::with_capacity(args.len() - 2);
    for id_arg in &args[2..] {
        match parse_id(id_arg, false) {
            Ok(id) => ids.push(id),
            Err(e) => return e,
        }
    }
    let stream = match db.get_stream_mut(&args[1]) {
        Ok(None) => return Frame::Integer(0),
        Ok(Some(s)) => s,
        Err(_) => return Frame::wrongtype(),
    };
    let mut n = 0i64;
    for id in ids {
        if stream.entries.remove(&id).is_some() {
            if id > stream.max_deleted_id {
                stream.max_deleted_id = id;
            }
            n += 1;
        }
    }
    Frame::Integer(n)
}

/// `XTRIM key MAXLEN|MINID [~|=] threshold [LIMIT c]`
pub fn xtrim(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() < 4 {
        return wrong_args("xtrim");
    }
    let spec_kind = upper(&args[2]);
    let mut i = 3;
    if matches!(args.get(i).map(|a| a.as_ref()), Some(b"~") | Some(b"=")) {
        i += 1;
    }
    let threshold = args.get(i).cloned().ok_or(());
    let threshold = match threshold {
        Ok(t) => t,
        Err(_) => return Frame::err("syntax error"),
    };
    i += 1;
    let mut limit: Option<i64> = None;
    if i < args.len() && upper(&args[i]) == "LIMIT" {
        match parse_int(args.get(i + 1).unwrap_or(&Bytes::new())) {
            Ok(n) => limit = Some(n),
            Err(e) => return e,
        }
        i += 2;
    }
    if i != args.len() {
        return Frame::err("syntax error");
    }

    let spec = match spec_kind.as_str() {
        "MAXLEN" => match parse_int(&threshold) {
            Ok(n) if n >= 0 => TrimSpec::MaxLen(n as usize, limit),
            Ok(_) => return Frame::err("MAXLEN can't be negative"),
            Err(e) => return e,
        },
        "MINID" => match parse_id(&threshold, true) {
            Ok(id) => TrimSpec::MinId(id, limit),
            Err(e) => return e,
        },
        _ => return Frame::err("syntax error"),
    };

    let stream = match db.get_stream_mut(&args[1]) {
        Ok(None) => return Frame::Integer(0),
        Ok(Some(s)) => s,
        Err(_) => return Frame::wrongtype(),
    };
    Frame::Integer(apply_trim(stream, spec) as i64)
}

// --- helpers ---

enum TrimSpec {
    MaxLen(usize, Option<i64>),
    MinId(StreamId, Option<i64>),
}

fn apply_trim(s: &mut Stream, spec: TrimSpec) -> usize {
    let mut removed = 0;
    match spec {
        TrimSpec::MaxLen(target, limit) => {
            while s.entries.len() > target {
                if let Some(cap) = limit {
                    if removed as i64 >= cap {
                        break;
                    }
                }
                let first = *s.entries.keys().next().unwrap();
                s.entries.remove(&first);
                if first > s.max_deleted_id {
                    s.max_deleted_id = first;
                }
                removed += 1;
            }
        }
        TrimSpec::MinId(min, limit) => {
            let to_remove: Vec<StreamId> = s
                .entries
                .range(..min)
                .map(|(id, _)| *id)
                .take(limit.map(|l| l as usize).unwrap_or(usize::MAX))
                .collect();
            for id in to_remove {
                s.entries.remove(&id);
                if id > s.max_deleted_id {
                    s.max_deleted_id = id;
                }
                removed += 1;
            }
        }
    }
    removed
}

/// Decide the id an `XADD` will use: `*` = auto (last_id.next or now-0),
/// `<ms>-*` = auto-seq within an explicit ms, otherwise the literal id.
fn resolve_add_id(id_arg: &[u8], stream: &Stream) -> Result<StreamId, Frame> {
    if id_arg == b"*" {
        let ms = now_ms();
        if ms > stream.last_id.ms {
            Ok(StreamId { ms, seq: 0 })
        } else {
            // Same ms or clock went backwards: bump the seq off last_id.
            stream
                .last_id
                .next()
                .ok_or_else(|| Frame::err("The stream has exhausted the last possible ID"))
        }
    } else if let Some(pos) = id_arg.iter().position(|&b| b == b'-') {
        let ms_part = str::from_utf8(&id_arg[..pos])
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or_else(|| Frame::err("Invalid stream ID specified as stream command argument"))?;
        let seq_part = &id_arg[pos + 1..];
        let id = if seq_part == b"*" {
            if stream.last_id.ms == ms_part {
                stream
                    .last_id
                    .next()
                    .ok_or_else(|| Frame::err("The stream has exhausted the last possible ID"))?
            } else {
                StreamId {
                    ms: ms_part,
                    seq: 0,
                }
            }
        } else {
            let seq = str::from_utf8(seq_part)
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .ok_or_else(|| {
                    Frame::err("Invalid stream ID specified as stream command argument")
                })?;
            StreamId { ms: ms_part, seq }
        };
        if id <= stream.last_id {
            return Err(Frame::err(
                "The ID specified in XADD is equal or smaller than the target stream top item",
            ));
        }
        Ok(id)
    } else {
        // Bare number is treated as `<ms>-0` (or `<ms>-*` for the auto case).
        let ms = str::from_utf8(id_arg)
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or_else(|| Frame::err("Invalid stream ID specified as stream command argument"))?;
        let id = StreamId { ms, seq: 0 };
        if id <= stream.last_id {
            return Err(Frame::err(
                "The ID specified in XADD is equal or smaller than the target stream top item",
            ));
        }
        Ok(id)
    }
}

/// Parse a strict `<ms>[-<seq>]` id used by `XRANGE`/`XDEL`/`XREAD`. `default_seq_max`
/// controls whether a bare `<ms>` defaults to `seq=0` (the low end of a range)
/// or `seq=u64::MAX` (the high end).
fn parse_id(arg: &[u8], default_seq_max: bool) -> Result<StreamId, Frame> {
    if arg == b"-" {
        return Ok(StreamId::MIN);
    }
    if arg == b"+" {
        return Ok(StreamId::MAX);
    }
    let s = str::from_utf8(arg)
        .map_err(|_| Frame::err("Invalid stream ID specified as stream command argument"))?;
    match s.split_once('-') {
        Some((ms_s, seq_s)) => {
            let ms = ms_s.parse::<u64>().map_err(|_| {
                Frame::err("Invalid stream ID specified as stream command argument")
            })?;
            let seq = seq_s.parse::<u64>().map_err(|_| {
                Frame::err("Invalid stream ID specified as stream command argument")
            })?;
            Ok(StreamId { ms, seq })
        }
        None => {
            let ms = s.parse::<u64>().map_err(|_| {
                Frame::err("Invalid stream ID specified as stream command argument")
            })?;
            let seq = if default_seq_max { u64::MAX } else { 0 };
            Ok(StreamId { ms, seq })
        }
    }
}

/// `XRANGE`/`XREVRANGE` accept `(id` for an exclusive bound, in addition to
/// the plain and `-`/`+` forms.
fn parse_range_bound(arg: &[u8], is_end: bool) -> Result<(StreamId, bool), Frame> {
    if let Some(rest) = arg.strip_prefix(b"(") {
        Ok((parse_id(rest, is_end)?, true))
    } else {
        Ok((parse_id(arg, is_end)?, false))
    }
}

/// Encode one stream entry as `[id, [f, v, f, v, ...]]`.
fn entry_frame(id: StreamId, fields: &[(Bytes, Bytes)]) -> Frame {
    let mut fv = Vec::with_capacity(fields.len() * 2);
    for (f, v) in fields {
        fv.push(Frame::Bulk(f.clone()));
        fv.push(Frame::Bulk(v.clone()));
    }
    Frame::Array(vec![
        Frame::Bulk(Bytes::from(id.to_string())),
        Frame::Array(fv),
    ])
}
