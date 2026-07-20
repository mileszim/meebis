//! Server/administrative commands. These are implemented well enough to keep
//! real clients and `redis-cli` happy, not to be faithful in every field.

use super::{upper, wrong_args};
use crate::db::{Db, Value};
use crate::resp::Frame;
use crate::server::Shared;
use bytes::Bytes;
use std::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn command(args: &[Bytes]) -> Frame {
    // We don't ship a full command table; return shapes clients tolerate.
    let sub = args.get(1).map(|a| upper(a)).unwrap_or_default();
    match sub.as_str() {
        "COUNT" => Frame::Integer(220),
        "DOCS" => Frame::Array(vec![]),
        "INFO" => Frame::Array(args[2..].iter().map(|_| Frame::NullArray).collect()),
        "LIST" => Frame::Array(vec![]),
        "GETKEYS" => Frame::err("The command has no key arguments"),
        "" => Frame::Array(vec![]),
        _ => Frame::Array(vec![]),
    }
}

pub fn config(shared: &Shared, args: &[Bytes]) -> Frame {
    if args.len() < 2 {
        return wrong_args("config");
    }
    match upper(&args[1]).as_str() {
        "GET" => {
            if args.len() < 3 {
                return wrong_args("config|get");
            }
            let cfg = shared.config.lock().unwrap();
            let mut pairs = Vec::new();
            for pat in &args[2..] {
                for (k, v) in cfg.iter() {
                    if crate::db::glob_match(pat, k.as_bytes()) {
                        pairs.push((Frame::bulk(k.clone()), Frame::bulk(v.clone())));
                    }
                }
            }
            Frame::Map(pairs)
        }
        "SET" => {
            if args.len() < 4 || args.len() % 2 != 0 {
                return wrong_args("config|set");
            }
            let mut cfg = shared.config.lock().unwrap();
            let mut i = 2;
            while i + 1 < args.len() {
                cfg.insert(
                    String::from_utf8_lossy(&args[i]).into_owned(),
                    String::from_utf8_lossy(&args[i + 1]).into_owned(),
                );
                i += 2;
            }
            Frame::ok()
        }
        "RESETSTAT" | "REWRITE" => Frame::ok(),
        other => Frame::err(format!(
            "Unknown CONFIG subcommand '{}'",
            other.to_lowercase()
        )),
    }
}

pub fn time() -> Frame {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    Frame::Array(vec![
        Frame::bulk(now.as_secs().to_string()),
        Frame::bulk(now.subsec_micros().to_string()),
    ])
}

pub fn info(shared: &Shared, conn: &crate::server::ConnState, keys: usize) -> Frame {
    let uptime = shared.start.elapsed().as_secs();
    let clients = shared.clients.lock().unwrap().len();
    let cmds = shared.commands_processed.load(Ordering::Relaxed);
    let conns = shared.connections_received.load(Ordering::Relaxed);

    let mut s = String::new();
    s.push_str("# Server\r\n");
    s.push_str("redis_version:7.4.0\r\n");
    s.push_str("redis_git_sha1:00000000\r\n");
    s.push_str("redis_git_dirty:0\r\n");
    s.push_str("redis_build_id:meebis\r\n");
    s.push_str("redis_mode:standalone\r\n");
    s.push_str(&format!("os:{}\r\n", std::env::consts::OS));
    s.push_str("arch_bits:64\r\n");
    s.push_str(&format!("process_id:{}\r\n", std::process::id()));
    s.push_str(&format!("run_id:{}\r\n", shared.run_id));
    s.push_str(&format!("tcp_port:{}\r\n", shared.port));
    s.push_str(&format!("uptime_in_seconds:{}\r\n", uptime));
    s.push_str(&format!("uptime_in_days:{}\r\n", uptime / 86400));
    s.push_str("\r\n# Clients\r\n");
    s.push_str(&format!("connected_clients:{}\r\n", clients.max(1)));
    s.push_str("cluster_connections:0\r\n");
    s.push_str("blocked_clients:0\r\n");
    s.push_str("\r\n# Memory\r\n");
    s.push_str("used_memory:1048576\r\n");
    s.push_str("used_memory_human:1.00M\r\n");
    s.push_str("used_memory_rss:1048576\r\n");
    s.push_str("maxmemory:0\r\n");
    s.push_str("maxmemory_policy:noeviction\r\n");
    s.push_str("mem_fragmentation_ratio:1.0\r\n");
    s.push_str("\r\n# Persistence\r\n");
    s.push_str("loading:0\r\n");
    s.push_str("rdb_changes_since_last_save:0\r\n");
    s.push_str("rdb_bgsave_in_progress:0\r\n");
    s.push_str("rdb_last_bgsave_status:ok\r\n");
    s.push_str("aof_enabled:0\r\n");
    s.push_str("aof_last_bgrewrite_status:ok\r\n");
    s.push_str("\r\n# Stats\r\n");
    s.push_str(&format!("total_connections_received:{}\r\n", conns));
    s.push_str(&format!("total_commands_processed:{}\r\n", cmds));
    s.push_str("instantaneous_ops_per_sec:0\r\n");
    s.push_str("expired_keys:0\r\n");
    s.push_str("evicted_keys:0\r\n");
    s.push_str("keyspace_hits:0\r\n");
    s.push_str("keyspace_misses:0\r\n");
    s.push_str("\r\n# Replication\r\n");
    s.push_str("role:master\r\n");
    s.push_str("connected_slaves:0\r\n");
    s.push_str("master_failover_state:no-failover\r\n");
    s.push_str("\r\n# CPU\r\n");
    s.push_str("used_cpu_sys:0.0\r\n");
    s.push_str("used_cpu_user:0.0\r\n");
    s.push_str("\r\n# Cluster\r\n");
    s.push_str("cluster_enabled:0\r\n");
    s.push_str("\r\n# Keyspace\r\n");
    if keys > 0 {
        s.push_str(&format!("db0:keys={},expires=0,avg_ttl=0\r\n", keys));
    }
    let _ = conn;
    Frame::Bulk(Bytes::from(s))
}

pub fn debug(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() < 2 {
        return wrong_args("debug");
    }
    match upper(&args[1]).as_str() {
        "OBJECT" => {
            if args.len() < 3 {
                return wrong_args("debug");
            }
            match db.get(&args[2]) {
                Some(v) => Frame::Simple(format!(
                    "Value at:0x0 refcount:1 encoding:{} serializedlength:8 lru:0 lru_seconds_idle:0",
                    encoding_of(v)
                )),
                None => Frame::err("no such key"),
            }
        }
        "JMAP"
        | "SET-ACTIVE-EXPIRE"
        | "QUICKLIST-PACKED-THRESHOLD"
        | "STRINGMATCH-LEN"
        | "CHANGE-REPL-ID"
        | "SLEEP"
        | "FLUSHALL"
        | "RELOAD" => Frame::ok(),
        _ => Frame::ok(),
    }
}

pub fn object(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() < 2 {
        return wrong_args("object");
    }
    match upper(&args[1]).as_str() {
        "ENCODING" => {
            if args.len() != 3 {
                return wrong_args("object|encoding");
            }
            match db.get(&args[2]) {
                Some(v) => Frame::Bulk(Bytes::from(encoding_of(v))),
                None => Frame::Null,
            }
        }
        "REFCOUNT" | "FREQ" => match args.get(2) {
            Some(k) if db.contains(k) => Frame::Integer(1),
            Some(_) => Frame::Null,
            None => wrong_args("object"),
        },
        "IDLETIME" => match args.get(2) {
            Some(k) if db.contains(k) => Frame::Integer(0),
            Some(_) => Frame::Null,
            None => wrong_args("object"),
        },
        "HELP" => Frame::Array(vec![Frame::bulk(
            "OBJECT ENCODING|REFCOUNT|IDLETIME|FREQ <key>",
        )]),
        other => Frame::err(format!(
            "Unknown OBJECT subcommand '{}'",
            other.to_lowercase()
        )),
    }
}

pub fn memory(db: &mut Db, args: &[Bytes]) -> Frame {
    if args.len() < 2 {
        return wrong_args("memory");
    }
    match upper(&args[1]).as_str() {
        "USAGE" => match args.get(2) {
            Some(k) => match db.get(k) {
                Some(v) => Frame::Integer(estimate_size(v) as i64),
                None => Frame::Null,
            },
            None => wrong_args("memory|usage"),
        },
        "DOCTOR" => Frame::bulk("Sam, I can't find any memory issue in your instance. I can only account for what occurs on this base.\n"),
        "PURGE" => Frame::ok(),
        "STATS" => Frame::Map(vec![
            (Frame::bulk("peak.allocated"), Frame::Integer(1048576)),
            (Frame::bulk("total.allocated"), Frame::Integer(1048576)),
            (Frame::bulk("keys.count"), Frame::Integer(db.len() as i64)),
        ]),
        other => Frame::err(format!("Unknown MEMORY subcommand '{}'", other.to_lowercase())),
    }
}

/// A plausible OBJECT ENCODING for a value, following Redis' small-vs-large
/// listpack/hashtable/skiplist conventions.
fn encoding_of(v: &Value) -> String {
    match v {
        Value::String(s) => {
            if std::str::from_utf8(s)
                .ok()
                .and_then(|x| x.parse::<i64>().ok())
                .is_some()
            {
                "int".into()
            } else if s.len() <= 44 {
                "embstr".into()
            } else {
                "raw".into()
            }
        }
        Value::List(_) => "listpack".into(),
        Value::Set(s) => {
            if s.iter().all(|m| {
                std::str::from_utf8(m)
                    .ok()
                    .and_then(|x| x.parse::<i64>().ok())
                    .is_some()
            }) && s.len() <= 512
            {
                "intset".into()
            } else if s.len() <= 128 {
                "listpack".into()
            } else {
                "hashtable".into()
            }
        }
        Value::Hash(h) => {
            if h.len() <= 128 {
                "listpack".into()
            } else {
                "hashtable".into()
            }
        }
        Value::ZSet(z) => {
            if z.len() <= 128 {
                "listpack".into()
            } else {
                "skiplist".into()
            }
        }
        Value::Stream(_) => "stream".into(),
    }
}

/// A rough byte estimate for MEMORY USAGE.
fn estimate_size(v: &Value) -> usize {
    let body = match v {
        Value::String(s) => s.len(),
        Value::List(l) => l.iter().map(|e| e.len() + 16).sum(),
        Value::Set(s) => s.iter().map(|e| e.len() + 16).sum(),
        Value::Hash(h) => h.iter().map(|(k, val)| k.len() + val.len() + 32).sum(),
        Value::ZSet(z) => z.iter_asc().map(|(m, _)| m.len() + 24).sum(),
        Value::Stream(s) => s
            .entries
            .values()
            .map(|fs| {
                fs.iter()
                    .map(|(f, v)| f.len() + v.len() + 32)
                    .sum::<usize>()
                    + 48
            })
            .sum(),
    };
    body + 64
}
