//! Command dispatch.
//!
//! [`handle`] is the entry point per request. It applies connection-level
//! policy (auth, subscribe-mode restrictions, transaction queuing) and then
//! delegates the actual data commands to [`execute`], which returns a single
//! reply [`Frame`].

mod admin;
mod bitops;
mod clientcmd;
mod generic;
mod hash;
mod list;
mod scripting;
mod set;
mod stream;
mod string;
mod zset;

use crate::db::Db;
use crate::resp::Frame;
use crate::server::{ConnState, Shared};
use bytes::Bytes;

/// What the connection loop should do with a handled command.
pub enum Reply {
    /// Write nothing (e.g. a blank inline line).
    None,
    /// Write a single frame.
    One(Frame),
    /// Write several frames in order (subscribe confirmations).
    Many(Vec<Frame>),
    /// Write the frame, then close the connection (`QUIT`).
    Close(Frame),
    /// Suspend the connection until data is available or the timeout expires;
    /// the loop re-runs the given command as the deadline evolves.
    Block(BlockReq),
}

/// A `BZPOPMIN`/`XREAD BLOCK`-style request the connection loop must drive.
pub struct BlockReq {
    /// Original command args, re-executed on each wake-up until data appears
    /// or the deadline passes.
    pub args: Vec<Bytes>,
    /// Absolute deadline in unix milliseconds. `None` means block forever.
    pub deadline_ms: Option<u64>,
    /// Reply to send if the deadline passes with no data (`NullArray` is the
    /// Redis convention for both `BZPOPMIN` and `XREAD BLOCK`).
    pub timeout_reply: Frame,
}

/// Commands permitted while a RESP2 connection is in subscribe mode.
fn allowed_in_subscribe(name: &str) -> bool {
    matches!(
        name,
        "SUBSCRIBE" | "UNSUBSCRIBE" | "PSUBSCRIBE" | "PUNSUBSCRIBE" | "PING" | "QUIT" | "RESET"
    )
}

pub fn handle(shared: &Shared, conn: &mut ConnState, args: Vec<Bytes>) -> Reply {
    if args.is_empty() {
        return Reply::None;
    }
    let name = String::from_utf8_lossy(&args[0]).to_ascii_uppercase();
    // Any command that isn't itself a block dispatches, then wakes waiters.
    // Cheaper than tracking write-ness per command; the loop only pays when
    // something is actually blocked (Notify does no work with zero waiters).
    let _wake = WakeOnDrop(&shared.write_notify, !is_blocking_cmd(&name));

    // Authentication gate.
    if shared.requirepass.is_some()
        && !conn.authenticated
        && !matches!(name.as_str(), "AUTH" | "HELLO" | "QUIT" | "RESET")
    {
        return Reply::One(Frame::Error("NOAUTH Authentication required.".into()));
    }

    // In RESP2 subscribe mode, only a handful of commands are legal.
    if !conn.resp3 && conn.subscription_count() > 0 && !allowed_in_subscribe(&name) {
        return Reply::One(Frame::err(format!(
            "Can't execute '{}': only (P|S)SUBSCRIBE / (P|S)UNSUBSCRIBE / PING / QUIT / RESET are allowed in this context",
            name.to_lowercase()
        )));
    }

    // Transaction control and queuing.
    match name.as_str() {
        "MULTI" => return Reply::One(multi(conn)),
        "EXEC" => return Reply::One(exec(shared, conn)),
        "DISCARD" => return Reply::One(discard(conn)),
        "WATCH" => return Reply::One(watch(shared, conn, &args)),
        "UNWATCH" => {
            conn.watched.clear();
            return Reply::One(Frame::ok());
        }
        _ => {}
    }

    if conn.in_multi {
        return Reply::One(queue_command(conn, &name, args));
    }

    // Connection-control commands that can produce multiple frames or close.
    match name.as_str() {
        "QUIT" => Reply::Close(Frame::ok()),
        "SUBSCRIBE" => Reply::Many(subscribe(shared, conn, &args, false)),
        "PSUBSCRIBE" => Reply::Many(subscribe(shared, conn, &args, true)),
        "UNSUBSCRIBE" => Reply::Many(unsubscribe(shared, conn, &args, false)),
        "PUNSUBSCRIBE" => Reply::Many(unsubscribe(shared, conn, &args, true)),
        "RESET" => Reply::One(reset(shared, conn)),
        "BZPOPMIN" => try_or_block_bzpop(shared, &args, true),
        "BZPOPMAX" => try_or_block_bzpop(shared, &args, false),
        "XREAD" => try_or_block_xread(shared, conn, args),
        _ => Reply::One(execute(shared, conn, &name, &args)),
    }
}

/// Handle a `BZPOPMIN`/`BZPOPMAX`: try once, and if every key is empty either
/// block (BLOCK/timeout supplied) or return the nil-array timeout reply.
fn try_or_block_bzpop(shared: &Shared, args: &[Bytes], min: bool) -> Reply {
    if args.len() < 3 {
        return Reply::One(wrong_args(if min { "bzpopmin" } else { "bzpopmax" }));
    }
    let timeout_arg = args.last().unwrap();
    let timeout_ms = match parse_timeout_ms(timeout_arg) {
        Ok(v) => v,
        Err(e) => return Reply::One(e),
    };
    let keys: Vec<Bytes> = args[1..args.len() - 1].to_vec();

    let mut db = shared.db.lock().unwrap();
    match zset::bzpop_try(&mut db, &keys, min) {
        Ok(Some(reply)) => Reply::One(reply),
        Err(e) => Reply::One(e),
        Ok(None) => Reply::Block(BlockReq {
            args: args.to_vec(),
            deadline_ms: timeout_ms.map(|ms| crate::db::now_ms().saturating_add(ms)),
            timeout_reply: Frame::NullArray,
        }),
    }
}

/// Handle `XREAD`: if `BLOCK` is set and there's no data yet, block; otherwise
/// let the normal dispatcher run it.
fn try_or_block_xread(shared: &Shared, conn: &mut ConnState, args: Vec<Bytes>) -> Reply {
    let block_ms = stream::xread_block_ms(&args);
    if block_ms.is_none() {
        return Reply::One(execute(shared, conn, "XREAD", &args));
    }
    let mut db = shared.db.lock().unwrap();
    // Snapshot `$` against the current last_id, so subsequent retries look
    // for entries added *after* now, not after their own new last_id.
    let resolved = match stream::resolve_dollar_ids(&mut db, &args) {
        Ok(v) => v,
        Err(e) => return Reply::One(e),
    };
    let first = stream::xread(&mut db, &resolved);
    if !matches!(first, Frame::NullArray) {
        return Reply::One(first);
    }
    let ms = block_ms.unwrap();
    Reply::Block(BlockReq {
        args: resolved,
        deadline_ms: if ms == 0 {
            None
        } else {
            Some(crate::db::now_ms().saturating_add(ms))
        },
        timeout_reply: Frame::NullArray,
    })
}

/// Parse a Redis `timeout` argument (seconds, may be fractional, `0` = block
/// forever). Returns `Some(ms)` for a positive timeout, `None` for `0`, or an
/// error frame on malformed input.
fn parse_timeout_ms(arg: &Bytes) -> Result<Option<u64>, Frame> {
    let s = std::str::from_utf8(arg)
        .map_err(|_| Frame::err("timeout is not a float or out of range"))?;
    let f = s
        .parse::<f64>()
        .map_err(|_| Frame::err("timeout is not a float or out of range"))?;
    if !f.is_finite() || f < 0.0 {
        return Err(Frame::err("timeout is negative"));
    }
    if f == 0.0 {
        Ok(None)
    } else {
        Ok(Some((f * 1000.0).round() as u64))
    }
}

/// Retry a blocked command (`BZPOPMIN`/`XREAD BLOCK`) once, non-blocking, and
/// return `Some(frame)` if it succeeded, `None` if the caller must keep waiting.
pub fn retry_block(shared: &Shared, conn: &mut ConnState, req: &BlockReq) -> Option<Frame> {
    let name = String::from_utf8_lossy(&req.args[0]).to_ascii_uppercase();
    let mut db = shared.db.lock().unwrap();
    match name.as_str() {
        "BZPOPMIN" | "BZPOPMAX" => {
            let min = name == "BZPOPMIN";
            let keys = &req.args[1..req.args.len() - 1];
            match zset::bzpop_try(&mut db, keys, min) {
                Ok(Some(r)) => Some(r),
                Ok(None) => None,
                Err(e) => Some(e),
            }
        }
        "XREAD" => {
            let frame = stream::xread(&mut db, &req.args);
            if matches!(frame, Frame::NullArray) {
                None
            } else {
                Some(frame)
            }
        }
        _ => {
            // Not a supported blocking command; fall back to running it non-blocking.
            Some(execute_locked(&mut db, shared, conn, &name, &req.args))
        }
    }
}

/// Run a single data/server/connection command and return its reply frame.
///
/// This acquires the keyspace lock once and delegates to [`execute_locked`], so
/// that a whole command (and, for `EVAL`, a whole script) sees a consistent
/// snapshot the way Redis' single thread does.
fn execute(shared: &Shared, conn: &mut ConnState, name: &str, args: &[Bytes]) -> Frame {
    let mut db = shared.db.lock().unwrap();
    execute_locked(&mut db, shared, conn, name, args)
}

/// Run a single command against an already-locked keyspace. Kept separate from
/// [`execute`] so `EVAL` can hold the lock across an entire script and route
/// each `redis.call` back here without re-locking (which would deadlock, since
/// the keyspace mutex is not reentrant).
pub(crate) fn execute_locked(
    db: &mut Db,
    shared: &Shared,
    conn: &mut ConnState,
    name: &str,
    args: &[Bytes],
) -> Frame {
    match name {
        // --- strings ---
        "GET" => string::get(db, args),
        "SET" => string::set(db, args),
        "SETNX" => string::setnx(db, args),
        "SETEX" => string::setex(db, args, false),
        "PSETEX" => string::setex(db, args, true),
        "GETSET" => string::getset(db, args),
        "GETDEL" => string::getdel(db, args),
        "GETEX" => string::getex(db, args),
        "APPEND" => string::append(db, args),
        "STRLEN" => string::strlen(db, args),
        "INCR" => string::incr(db, args, 1),
        "DECR" => string::incr(db, args, -1),
        "INCRBY" => string::incrby(db, args, false),
        "DECRBY" => string::incrby(db, args, true),
        "INCRBYFLOAT" => string::incrbyfloat(db, args),
        "MGET" => string::mget(db, args),
        "MSET" => string::mset(db, args),
        "MSETNX" => string::msetnx(db, args),
        "GETRANGE" | "SUBSTR" => string::getrange(db, args),
        "SETRANGE" => string::setrange(db, args),

        // --- bitmaps ---
        "SETBIT" => bitops::setbit(db, args),
        "GETBIT" => bitops::getbit(db, args),
        "BITCOUNT" => bitops::bitcount(db, args),
        "BITPOS" => bitops::bitpos(db, args),
        "BITOP" => bitops::bitop(db, args),

        // --- generic / keyspace ---
        "DEL" | "UNLINK" => generic::del(db, args),
        "EXISTS" => generic::exists(db, args),
        "EXPIRE" => generic::expire(db, args, 1000, true),
        "PEXPIRE" => generic::expire(db, args, 1, true),
        "EXPIREAT" => generic::expire(db, args, 1000, false),
        "PEXPIREAT" => generic::expire(db, args, 1, false),
        "TTL" => generic::ttl(db, args, true),
        "PTTL" => generic::ttl(db, args, false),
        "EXPIRETIME" => generic::expiretime(db, args, true),
        "PEXPIRETIME" => generic::expiretime(db, args, false),
        "PERSIST" => generic::persist(db, args),
        "KEYS" => generic::keys(db, args),
        "SCAN" => generic::scan(db, args),
        "TYPE" => generic::type_cmd(db, args),
        "RENAME" => generic::rename(db, args, false),
        "RENAMENX" => generic::rename(db, args, true),
        "RANDOMKEY" => generic::randomkey(db, args),
        "TOUCH" => generic::exists(db, args),
        "COPY" => generic::copy(db, args),

        // --- hashes ---
        "HSET" | "HMSET" => hash::hset(db, args, name == "HMSET"),
        "HSETNX" => hash::hsetnx(db, args),
        "HGET" => hash::hget(db, args),
        "HMGET" => hash::hmget(db, args),
        "HDEL" => hash::hdel(db, args),
        "HGETALL" => hash::hgetall(db, args),
        "HKEYS" => hash::hkeys(db, args),
        "HVALS" => hash::hvals(db, args),
        "HLEN" => hash::hlen(db, args),
        "HEXISTS" => hash::hexists(db, args),
        "HSTRLEN" => hash::hstrlen(db, args),
        "HINCRBY" => hash::hincrby(db, args),
        "HINCRBYFLOAT" => hash::hincrbyfloat(db, args),
        "HSCAN" => hash::hscan(db, args),
        "HRANDFIELD" => hash::hrandfield(db, args),

        // --- lists ---
        "LPUSH" => list::push(db, args, true, false),
        "RPUSH" => list::push(db, args, false, false),
        "LPUSHX" => list::push(db, args, true, true),
        "RPUSHX" => list::push(db, args, false, true),
        "LPOP" => list::pop(db, args, true),
        "RPOP" => list::pop(db, args, false),
        "LLEN" => list::llen(db, args),
        "LRANGE" => list::lrange(db, args),
        "LINDEX" => list::lindex(db, args),
        "LSET" => list::lset(db, args),
        "LREM" => list::lrem(db, args),
        "LTRIM" => list::ltrim(db, args),
        "LINSERT" => list::linsert(db, args),
        "LPOS" => list::lpos(db, args),
        "RPOPLPUSH" => list::rpoplpush(db, args),
        "LMOVE" => list::lmove(db, args),

        // --- sets ---
        "SADD" => set::sadd(db, args),
        "SREM" => set::srem(db, args),
        "SMEMBERS" => set::smembers(db, args),
        "SISMEMBER" => set::sismember(db, args),
        "SMISMEMBER" => set::smismember(db, args),
        "SCARD" => set::scard(db, args),
        "SPOP" => set::spop(db, args),
        "SRANDMEMBER" => set::srandmember(db, args),
        "SMOVE" => set::smove(db, args),
        "SUNION" => set::setop(db, args, set::Op::Union, false),
        "SINTER" => set::setop(db, args, set::Op::Inter, false),
        "SDIFF" => set::setop(db, args, set::Op::Diff, false),
        "SUNIONSTORE" => set::setop(db, args, set::Op::Union, true),
        "SINTERSTORE" => set::setop(db, args, set::Op::Inter, true),
        "SDIFFSTORE" => set::setop(db, args, set::Op::Diff, true),
        "SINTERCARD" => set::sintercard(db, args),
        "SSCAN" => set::sscan(db, args),

        // --- sorted sets ---
        "ZADD" => zset::zadd(db, args),
        "ZREM" => zset::zrem(db, args),
        "ZSCORE" => zset::zscore(db, args),
        "ZMSCORE" => zset::zmscore(db, args),
        "ZCARD" => zset::zcard(db, args),
        "ZCOUNT" => zset::zcount(db, args),
        "ZINCRBY" => zset::zincrby(db, args),
        "ZRANK" => zset::zrank(db, args, false),
        "ZREVRANK" => zset::zrank(db, args, true),
        "ZRANGE" => zset::zrange(db, args, false),
        "ZREVRANGE" => zset::zrange(db, args, true),
        "ZRANGEBYSCORE" => zset::zrangebyscore(db, args, false),
        "ZREVRANGEBYSCORE" => zset::zrangebyscore(db, args, true),
        "ZRANGEBYLEX" => zset::zrangebylex(db, args, false),
        "ZREVRANGEBYLEX" => zset::zrangebylex(db, args, true),
        "ZLEXCOUNT" => zset::zlexcount(db, args),
        "ZPOPMIN" => zset::zpop(db, args, true),
        "ZPOPMAX" => zset::zpop(db, args, false),
        "ZREMRANGEBYRANK" => zset::zremrangebyrank(db, args),
        "ZREMRANGEBYSCORE" => zset::zremrangebyscore(db, args),
        "ZSCAN" => zset::zscan(db, args),
        "ZRANDMEMBER" => zset::zrandmember(db, args),

        // --- connection ---
        "PING" => clientcmd::ping(args),
        "ECHO" => clientcmd::echo(args),
        "HELLO" => clientcmd::hello(shared, conn, args),
        "AUTH" => clientcmd::auth(shared, conn, args),
        "SELECT" => clientcmd::select(args),
        "CLIENT" => clientcmd::client(shared, conn, args),

        // --- pub/sub (non-subscribe) ---
        "PUBLISH" | "SPUBLISH" => publish(shared, args),
        "PUBSUB" => pubsub_cmd(shared, args),

        // --- server / admin ---
        "COMMAND" => admin::command(args),
        "CONFIG" => admin::config(shared, args),
        "INFO" => admin::info(shared, conn, db.len()),
        "DBSIZE" => Frame::Integer(db.len() as i64),
        "FLUSHDB" | "FLUSHALL" => {
            db.clear();
            Frame::ok()
        }

        // --- streams ---
        "XADD" => stream::xadd(db, args),
        "XLEN" => stream::xlen(db, args),
        "XRANGE" => stream::xrange(db, args, false),
        "XREVRANGE" => stream::xrange(db, args, true),
        "XREAD" => stream::xread(db, args),
        "XDEL" => stream::xdel(db, args),
        "XTRIM" => stream::xtrim(db, args),

        // --- scripting ---
        "EVAL" | "EVAL_RO" => scripting::eval(db, shared, conn, args, scripting::Source::Body),
        "EVALSHA" | "EVALSHA_RO" => scripting::eval(db, shared, conn, args, scripting::Source::Sha),
        "SCRIPT" => scripting::script(shared, args),
        "TIME" => admin::time(),
        "DEBUG" => admin::debug(db, args),
        "OBJECT" => admin::object(db, args),
        "WAIT" => Frame::Integer(0),
        "LOLWUT" => Frame::bulk("meebis: a disposable Redis for ephemeral dev work\n"),
        "SAVE" | "BGSAVE" | "BGREWRITEAOF" => Frame::ok(),
        "LASTSAVE" => Frame::Integer((crate::db::now_ms() / 1000) as i64),
        "SWAPDB" => Frame::ok(),
        "MEMORY" => admin::memory(db, args),
        "SHUTDOWN" => {
            // A dev tool honoring SHUTDOWN simply exits.
            std::process::exit(0);
        }

        other => Frame::err(format!(
            "unknown command '{}', with args beginning with: {}",
            other.to_lowercase(),
            args.get(1)
                .map(|a| format!("'{}'", String::from_utf8_lossy(a)))
                .unwrap_or_default()
        )),
    }
}

// --- transactions ---

fn multi(conn: &mut ConnState) -> Frame {
    if conn.in_multi {
        return Frame::err("MULTI calls can not be nested");
    }
    conn.in_multi = true;
    conn.multi_error = false;
    conn.multi_queue.clear();
    Frame::ok()
}

fn discard(conn: &mut ConnState) -> Frame {
    if !conn.in_multi {
        return Frame::err("DISCARD without MULTI");
    }
    conn.in_multi = false;
    conn.multi_error = false;
    conn.multi_queue.clear();
    conn.watched.clear();
    Frame::ok()
}

fn queue_command(conn: &mut ConnState, name: &str, args: Vec<Bytes>) -> Frame {
    if matches!(
        name,
        "SUBSCRIBE" | "UNSUBSCRIBE" | "PSUBSCRIBE" | "PUNSUBSCRIBE"
    ) {
        conn.multi_error = true;
        return Frame::err(format!("{} is not allowed in transactions", name));
    }
    conn.multi_queue.push(args);
    Frame::Simple("QUEUED".into())
}

fn watch(shared: &Shared, conn: &mut ConnState, args: &[Bytes]) -> Frame {
    if args.len() < 2 {
        return wrong_args("watch");
    }
    if conn.in_multi {
        return Frame::err("WATCH inside MULTI is not allowed");
    }
    {
        let mut db = shared.db.lock().unwrap();
        for key in &args[1..] {
            let fp = db.fingerprint(key);
            conn.watched
                .insert(key.clone(), (fp.is_some(), fp.unwrap_or(0)));
        }
    }
    Frame::ok()
}

fn exec(shared: &Shared, conn: &mut ConnState) -> Frame {
    if !conn.in_multi {
        return Frame::err("EXEC without MULTI");
    }
    conn.in_multi = false;
    let queue = std::mem::take(&mut conn.multi_queue);
    let watched = std::mem::take(&mut conn.watched);

    if conn.multi_error {
        conn.multi_error = false;
        return Frame::Error("EXECABORT Transaction discarded because of previous errors.".into());
    }

    // Abort if any watched key changed since it was watched.
    if !watched.is_empty() {
        let mut db = shared.db.lock().unwrap();
        for (key, snapshot) in &watched {
            let fp = db.fingerprint(key);
            let current = (fp.is_some(), fp.unwrap_or(0));
            if current != *snapshot {
                return Frame::NullArray;
            }
        }
    }

    let mut replies = Vec::with_capacity(queue.len());
    for cmd in queue {
        let cname = String::from_utf8_lossy(&cmd[0]).to_ascii_uppercase();
        replies.push(execute(shared, conn, &cname, &cmd));
    }
    Frame::Array(replies)
}

fn reset(shared: &Shared, conn: &mut ConnState) -> Frame {
    conn.in_multi = false;
    conn.multi_error = false;
    conn.multi_queue.clear();
    conn.watched.clear();
    for ch in conn.subscribed_channels.drain() {
        shared.pubsub.unsubscribe(&ch, conn.id);
    }
    for pat in conn.subscribed_patterns.drain() {
        shared.pubsub.punsubscribe(&pat, conn.id);
    }
    conn.resp3 = false;
    conn.name = Bytes::new();
    if shared.requirepass.is_some() {
        conn.authenticated = false;
    }
    Frame::Simple("RESET".into())
}

// --- pub/sub ---

fn subscribe(shared: &Shared, conn: &mut ConnState, args: &[Bytes], pattern: bool) -> Vec<Frame> {
    if args.len() < 2 {
        let n = if pattern { "psubscribe" } else { "subscribe" };
        return vec![wrong_args(n)];
    }
    let kind = if pattern { "psubscribe" } else { "subscribe" };
    let mut out = Vec::new();
    for target in &args[1..] {
        if pattern {
            if conn.subscribed_patterns.insert(target.clone()) {
                shared.pubsub.psubscribe(target.clone(), conn.id, &conn.tx);
            }
        } else if conn.subscribed_channels.insert(target.clone()) {
            shared.pubsub.subscribe(target.clone(), conn.id, &conn.tx);
        }
        out.push(Frame::Push(vec![
            Frame::bulk(kind),
            Frame::bulk(target.clone()),
            Frame::Integer(conn.subscription_count() as i64),
        ]));
    }
    out
}

fn unsubscribe(shared: &Shared, conn: &mut ConnState, args: &[Bytes], pattern: bool) -> Vec<Frame> {
    let kind = if pattern {
        "punsubscribe"
    } else {
        "unsubscribe"
    };
    // Determine the targets: explicit list, or everything currently subscribed.
    let targets: Vec<Bytes> = if args.len() > 1 {
        args[1..].to_vec()
    } else if pattern {
        conn.subscribed_patterns.iter().cloned().collect()
    } else {
        conn.subscribed_channels.iter().cloned().collect()
    };

    if targets.is_empty() {
        // Redis still emits one acknowledgement with a nil channel.
        return vec![Frame::Push(vec![
            Frame::bulk(kind),
            Frame::Null,
            Frame::Integer(conn.subscription_count() as i64),
        ])];
    }

    let mut out = Vec::new();
    for target in targets {
        if pattern {
            if conn.subscribed_patterns.remove(&target) {
                shared.pubsub.punsubscribe(&target, conn.id);
            }
        } else if conn.subscribed_channels.remove(&target) {
            shared.pubsub.unsubscribe(&target, conn.id);
        }
        out.push(Frame::Push(vec![
            Frame::bulk(kind),
            Frame::bulk(target),
            Frame::Integer(conn.subscription_count() as i64),
        ]));
    }
    out
}

fn publish(shared: &Shared, args: &[Bytes]) -> Frame {
    if args.len() != 3 {
        return wrong_args("publish");
    }
    Frame::Integer(shared.pubsub.publish(&args[1], &args[2]))
}

fn pubsub_cmd(shared: &Shared, args: &[Bytes]) -> Frame {
    if args.len() < 2 {
        return wrong_args("pubsub");
    }
    let sub = String::from_utf8_lossy(&args[1]).to_ascii_uppercase();
    match sub.as_str() {
        "CHANNELS" => {
            let pat = args.get(2).map(|b| b.as_ref());
            Frame::Array(
                shared
                    .pubsub
                    .channels(pat)
                    .into_iter()
                    .map(Frame::Bulk)
                    .collect(),
            )
        }
        "NUMSUB" => {
            let mut out = Vec::new();
            for ch in &args[2..] {
                out.push(Frame::Bulk(ch.clone()));
                out.push(Frame::Integer(shared.pubsub.numsub(ch)));
            }
            Frame::Array(out)
        }
        "NUMPAT" => Frame::Integer(shared.pubsub.numpat()),
        other => Frame::err(format!(
            "Unknown PUBSUB subcommand '{}'",
            other.to_lowercase()
        )),
    }
}

// --- shared helpers used across command modules ---

/// The canonical "wrong number of arguments" error.
pub(crate) fn wrong_args(name: &str) -> Frame {
    Frame::Error(format!(
        "ERR wrong number of arguments for '{}' command",
        name.to_lowercase()
    ))
}

/// Parse a strict signed integer argument.
pub(crate) fn parse_int(b: &[u8]) -> Result<i64, Frame> {
    std::str::from_utf8(b)
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .ok_or_else(|| Frame::err("value is not an integer or out of range"))
}

/// Parse a float argument, accepting `inf`/`-inf` and rejecting NaN.
pub(crate) fn parse_float(b: &[u8]) -> Result<f64, Frame> {
    let s = std::str::from_utf8(b).ok().map(|s| s.trim()).unwrap_or("");
    if s.is_empty() {
        return Err(Frame::err("value is not a valid float"));
    }
    match s.parse::<f64>() {
        Ok(f) if !f.is_nan() => Ok(f),
        _ => Err(Frame::err("value is not a valid float")),
    }
}

/// Normalize a possibly-negative index against a container length, as Redis
/// does for `LINDEX`, `LRANGE`, etc. Returns an index that may be out of range.
pub(crate) fn norm_index(idx: i64, len: usize) -> i64 {
    if idx < 0 {
        idx + len as i64
    } else {
        idx
    }
}

/// Uppercased UTF-8 view of an argument (for option keywords).
pub(crate) fn upper(b: &[u8]) -> String {
    String::from_utf8_lossy(b).to_ascii_uppercase()
}

fn is_blocking_cmd(name: &str) -> bool {
    matches!(name, "BZPOPMIN" | "BZPOPMAX" | "BLPOP" | "BRPOP")
    // XREAD may or may not block; the branch that turns into a Block
    // returns Reply::Block before this wake would fire.
}

/// Signal `write_notify` on drop unless suppressed, so blocking commands wake
/// up. Placed in `handle` around the whole dispatch — cheap when nothing's
/// blocked (Notify skips work with zero waiters).
struct WakeOnDrop<'a>(&'a tokio::sync::Notify, bool);

impl Drop for WakeOnDrop<'_> {
    fn drop(&mut self) {
        if self.1 {
            self.0.notify_waiters();
        }
    }
}

/// A fast thread-local xorshift PRNG, seeded lazily from the clock. Used for
/// the handful of commands with random behavior (SPOP, SRANDMEMBER, RANDOMKEY,
/// HRANDFIELD). Not cryptographic — this is a dev tool.
pub(crate) fn rand_u64() -> u64 {
    use std::cell::Cell;
    use std::time::{SystemTime, UNIX_EPOCH};
    thread_local!(static STATE: Cell<u64> = const { Cell::new(0) });
    STATE.with(|c| {
        let mut x = c.get();
        if x == 0 {
            x = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0x9E37_79B9_7F4A_7C15)
                | 1;
        }
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        c.set(x);
        x
    })
}
