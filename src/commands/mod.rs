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
mod set;
mod string;
mod zset;

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
}

/// Lock the shared keyspace. The guard never crosses an `.await`.
macro_rules! db {
    ($s:expr) => {
        &mut *$s.db.lock().unwrap()
    };
}

/// Commands permitted while a RESP2 connection is in subscribe mode.
fn allowed_in_subscribe(name: &str) -> bool {
    matches!(
        name,
        "SUBSCRIBE"
            | "UNSUBSCRIBE"
            | "PSUBSCRIBE"
            | "PUNSUBSCRIBE"
            | "PING"
            | "QUIT"
            | "RESET"
    )
}

pub fn handle(shared: &Shared, conn: &mut ConnState, args: Vec<Bytes>) -> Reply {
    if args.is_empty() {
        return Reply::None;
    }
    let name = String::from_utf8_lossy(&args[0]).to_ascii_uppercase();

    // Authentication gate.
    if shared.requirepass.is_some()
        && !conn.authenticated
        && !matches!(name.as_str(), "AUTH" | "HELLO" | "QUIT" | "RESET")
    {
        return Reply::One(Frame::Error(
            "NOAUTH Authentication required.".into(),
        ));
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
        _ => Reply::One(execute(shared, conn, &name, &args)),
    }
}

/// Run a single data/server/connection command and return its reply frame.
fn execute(shared: &Shared, conn: &mut ConnState, name: &str, args: &[Bytes]) -> Frame {
    match name {
        // --- strings ---
        "GET" => string::get(db!(shared), args),
        "SET" => string::set(db!(shared), args),
        "SETNX" => string::setnx(db!(shared), args),
        "SETEX" => string::setex(db!(shared), args, false),
        "PSETEX" => string::setex(db!(shared), args, true),
        "GETSET" => string::getset(db!(shared), args),
        "GETDEL" => string::getdel(db!(shared), args),
        "GETEX" => string::getex(db!(shared), args),
        "APPEND" => string::append(db!(shared), args),
        "STRLEN" => string::strlen(db!(shared), args),
        "INCR" => string::incr(db!(shared), args, 1),
        "DECR" => string::incr(db!(shared), args, -1),
        "INCRBY" => string::incrby(db!(shared), args, false),
        "DECRBY" => string::incrby(db!(shared), args, true),
        "INCRBYFLOAT" => string::incrbyfloat(db!(shared), args),
        "MGET" => string::mget(db!(shared), args),
        "MSET" => string::mset(db!(shared), args),
        "MSETNX" => string::msetnx(db!(shared), args),
        "GETRANGE" | "SUBSTR" => string::getrange(db!(shared), args),
        "SETRANGE" => string::setrange(db!(shared), args),

        // --- bitmaps ---
        "SETBIT" => bitops::setbit(db!(shared), args),
        "GETBIT" => bitops::getbit(db!(shared), args),
        "BITCOUNT" => bitops::bitcount(db!(shared), args),
        "BITPOS" => bitops::bitpos(db!(shared), args),
        "BITOP" => bitops::bitop(db!(shared), args),

        // --- generic / keyspace ---
        "DEL" | "UNLINK" => generic::del(db!(shared), args),
        "EXISTS" => generic::exists(db!(shared), args),
        "EXPIRE" => generic::expire(db!(shared), args, 1000, true),
        "PEXPIRE" => generic::expire(db!(shared), args, 1, true),
        "EXPIREAT" => generic::expire(db!(shared), args, 1000, false),
        "PEXPIREAT" => generic::expire(db!(shared), args, 1, false),
        "TTL" => generic::ttl(db!(shared), args, true),
        "PTTL" => generic::ttl(db!(shared), args, false),
        "EXPIRETIME" => generic::expiretime(db!(shared), args, true),
        "PEXPIRETIME" => generic::expiretime(db!(shared), args, false),
        "PERSIST" => generic::persist(db!(shared), args),
        "KEYS" => generic::keys(db!(shared), args),
        "SCAN" => generic::scan(db!(shared), args),
        "TYPE" => generic::type_cmd(db!(shared), args),
        "RENAME" => generic::rename(db!(shared), args, false),
        "RENAMENX" => generic::rename(db!(shared), args, true),
        "RANDOMKEY" => generic::randomkey(db!(shared), args),
        "TOUCH" => generic::exists(db!(shared), args),
        "COPY" => generic::copy(db!(shared), args),

        // --- hashes ---
        "HSET" | "HMSET" => hash::hset(db!(shared), args, name == "HMSET"),
        "HSETNX" => hash::hsetnx(db!(shared), args),
        "HGET" => hash::hget(db!(shared), args),
        "HMGET" => hash::hmget(db!(shared), args),
        "HDEL" => hash::hdel(db!(shared), args),
        "HGETALL" => hash::hgetall(db!(shared), args),
        "HKEYS" => hash::hkeys(db!(shared), args),
        "HVALS" => hash::hvals(db!(shared), args),
        "HLEN" => hash::hlen(db!(shared), args),
        "HEXISTS" => hash::hexists(db!(shared), args),
        "HSTRLEN" => hash::hstrlen(db!(shared), args),
        "HINCRBY" => hash::hincrby(db!(shared), args),
        "HINCRBYFLOAT" => hash::hincrbyfloat(db!(shared), args),
        "HSCAN" => hash::hscan(db!(shared), args),
        "HRANDFIELD" => hash::hrandfield(db!(shared), args),

        // --- lists ---
        "LPUSH" => list::push(db!(shared), args, true, false),
        "RPUSH" => list::push(db!(shared), args, false, false),
        "LPUSHX" => list::push(db!(shared), args, true, true),
        "RPUSHX" => list::push(db!(shared), args, false, true),
        "LPOP" => list::pop(db!(shared), args, true),
        "RPOP" => list::pop(db!(shared), args, false),
        "LLEN" => list::llen(db!(shared), args),
        "LRANGE" => list::lrange(db!(shared), args),
        "LINDEX" => list::lindex(db!(shared), args),
        "LSET" => list::lset(db!(shared), args),
        "LREM" => list::lrem(db!(shared), args),
        "LTRIM" => list::ltrim(db!(shared), args),
        "LINSERT" => list::linsert(db!(shared), args),
        "LPOS" => list::lpos(db!(shared), args),
        "RPOPLPUSH" => list::rpoplpush(db!(shared), args),
        "LMOVE" => list::lmove(db!(shared), args),

        // --- sets ---
        "SADD" => set::sadd(db!(shared), args),
        "SREM" => set::srem(db!(shared), args),
        "SMEMBERS" => set::smembers(db!(shared), args),
        "SISMEMBER" => set::sismember(db!(shared), args),
        "SMISMEMBER" => set::smismember(db!(shared), args),
        "SCARD" => set::scard(db!(shared), args),
        "SPOP" => set::spop(db!(shared), args),
        "SRANDMEMBER" => set::srandmember(db!(shared), args),
        "SMOVE" => set::smove(db!(shared), args),
        "SUNION" => set::setop(db!(shared), args, set::Op::Union, false),
        "SINTER" => set::setop(db!(shared), args, set::Op::Inter, false),
        "SDIFF" => set::setop(db!(shared), args, set::Op::Diff, false),
        "SUNIONSTORE" => set::setop(db!(shared), args, set::Op::Union, true),
        "SINTERSTORE" => set::setop(db!(shared), args, set::Op::Inter, true),
        "SDIFFSTORE" => set::setop(db!(shared), args, set::Op::Diff, true),
        "SINTERCARD" => set::sintercard(db!(shared), args),
        "SSCAN" => set::sscan(db!(shared), args),

        // --- sorted sets ---
        "ZADD" => zset::zadd(db!(shared), args),
        "ZREM" => zset::zrem(db!(shared), args),
        "ZSCORE" => zset::zscore(db!(shared), args),
        "ZMSCORE" => zset::zmscore(db!(shared), args),
        "ZCARD" => zset::zcard(db!(shared), args),
        "ZCOUNT" => zset::zcount(db!(shared), args),
        "ZINCRBY" => zset::zincrby(db!(shared), args),
        "ZRANK" => zset::zrank(db!(shared), args, false),
        "ZREVRANK" => zset::zrank(db!(shared), args, true),
        "ZRANGE" => zset::zrange(db!(shared), args, false),
        "ZREVRANGE" => zset::zrange(db!(shared), args, true),
        "ZRANGEBYSCORE" => zset::zrangebyscore(db!(shared), args, false),
        "ZREVRANGEBYSCORE" => zset::zrangebyscore(db!(shared), args, true),
        "ZRANGEBYLEX" => zset::zrangebylex(db!(shared), args, false),
        "ZREVRANGEBYLEX" => zset::zrangebylex(db!(shared), args, true),
        "ZLEXCOUNT" => zset::zlexcount(db!(shared), args),
        "ZPOPMIN" => zset::zpop(db!(shared), args, true),
        "ZPOPMAX" => zset::zpop(db!(shared), args, false),
        "ZREMRANGEBYRANK" => zset::zremrangebyrank(db!(shared), args),
        "ZREMRANGEBYSCORE" => zset::zremrangebyscore(db!(shared), args),
        "ZSCAN" => zset::zscan(db!(shared), args),
        "ZRANDMEMBER" => zset::zrandmember(db!(shared), args),

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
        "INFO" => admin::info(shared, conn),
        "DBSIZE" => Frame::Integer(shared.db.lock().unwrap().len() as i64),
        "FLUSHDB" | "FLUSHALL" => {
            db!(shared).clear();
            Frame::ok()
        }
        "TIME" => admin::time(),
        "DEBUG" => admin::debug(db!(shared), args),
        "OBJECT" => admin::object(db!(shared), args),
        "WAIT" => Frame::Integer(0),
        "LOLWUT" => Frame::bulk("meebis: a disposable Redis for ephemeral dev work\n"),
        "SAVE" | "BGSAVE" | "BGREWRITEAOF" => Frame::ok(),
        "LASTSAVE" => Frame::Integer((crate::db::now_ms() / 1000) as i64),
        "SWAPDB" => Frame::ok(),
        "MEMORY" => admin::memory(db!(shared), args),
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
        return Frame::Error(
            "EXECABORT Transaction discarded because of previous errors.".into(),
        );
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

fn unsubscribe(
    shared: &Shared,
    conn: &mut ConnState,
    args: &[Bytes],
    pattern: bool,
) -> Vec<Frame> {
    let kind = if pattern { "punsubscribe" } else { "unsubscribe" };
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
        other => Frame::err(format!("Unknown PUBSUB subcommand '{}'", other.to_lowercase())),
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
