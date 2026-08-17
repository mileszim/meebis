//! Process-wide shared state and per-connection state.

use crate::db::Keyspace;
use crate::pubsub::PubSub;
use crate::resp::Frame;
use bytes::Bytes;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::Notify;

/// State shared across every connection. Cheap to `Arc`-clone.
pub struct Shared {
    /// Every numbered database behind one lock. See [`Keyspace`] for why they
    /// share a mutex rather than holding one each.
    pub db: Mutex<Keyspace>,
    pub pubsub: PubSub,
    /// Optional password; when set, connections must `AUTH` before issuing
    /// most commands.
    pub requirepass: Option<String>,
    /// Free-form config store backing `CONFIG GET`/`CONFIG SET`.
    pub config: Mutex<HashMap<String, String>>,
    /// Cache of Lua scripts, keyed by the lowercase-hex SHA-1 of the body, as
    /// populated by `SCRIPT LOAD` / `EVAL` and consulted by `EVALSHA`.
    pub scripts: Mutex<HashMap<String, Bytes>>,
    /// Notified whenever a write command runs; blocking commands (`BZPOPMIN`,
    /// `XREAD BLOCK`) wait on this instead of sleeping so they wake as soon as
    /// new data may be available.
    pub write_notify: Notify,
    /// Registry of live clients, for `CLIENT LIST`.
    pub clients: Mutex<HashMap<u64, ClientInfo>>,
    /// 40-hex-char identifier reported by `INFO`, regenerated each boot.
    pub run_id: String,
    /// Commands processed since boot (for `INFO`).
    pub commands_processed: AtomicU64,
    /// Connections accepted since boot (for `INFO`).
    pub connections_received: AtomicU64,
    next_client_id: AtomicU64,
    /// Whether every command and reply is logged (`--verbose`, or
    /// `CONFIG SET loglevel verbose`). Read on every command, so it lives in an
    /// atomic rather than behind the config mutex.
    verbose: AtomicBool,
    /// Where the RDB snapshot lives, when `--dumpfile` (or `--dir` /
    /// `--dbfilename`) asked for one. `None` restores the original behavior:
    /// nothing is read at boot and nothing is written at exit.
    pub dumpfile: Option<PathBuf>,
    /// Unix seconds of the last successful save, for `LASTSAVE` and `INFO`.
    /// Seeded with boot time, exactly as Redis does.
    last_save: AtomicU64,
    pub port: u16,
    /// The unix socket this server is listening on, when `--unixsocket` asked
    /// for one. Kept so the exit paths can unlink it.
    pub unixsocket: Option<PathBuf>,
    pub maxclients: usize,
    pub start: Instant,
}

impl Shared {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        requirepass: Option<String>,
        port: u16,
        unixsocket: Option<PathBuf>,
        maxclients: usize,
        databases: usize,
        verbose: bool,
        dumpfile: Option<PathBuf>,
        start: Instant,
    ) -> Shared {
        let mut config = HashMap::new();
        let databases_str = databases.to_string();

        // `dir` and `dbfilename` are probed by tooling even when persistence is
        // off, so report Redis' defaults in that case rather than nothing.
        let (dir, dbfilename) = match &dumpfile {
            Some(p) => (
                p.parent()
                    .filter(|d| !d.as_os_str().is_empty())
                    .unwrap_or(std::path::Path::new("."))
                    .to_string_lossy()
                    .into_owned(),
                p.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
            ),
            None => (
                std::env::current_dir()
                    .map(|d| d.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| ".".to_string()),
                "dump.rdb".to_string(),
            ),
        };

        for (k, v) in [
            ("maxmemory", "0"),
            ("maxmemory-policy", "noeviction"),
            ("save", ""),
            ("appendonly", "no"),
            ("appendfsync", "everysec"),
            ("databases", databases_str.as_str()),
            ("maxclients", "10000"),
            ("timeout", "0"),
            ("tcp-keepalive", "300"),
            ("loglevel", if verbose { "verbose" } else { "notice" }),
            ("dir", dir.as_str()),
            ("dbfilename", dbfilename.as_str()),
            // Redis reports this whether or not a socket is in use, with the
            // empty string standing for "TCP only".
            (
                "unixsocket",
                unixsocket
                    .as_ref()
                    .map(|p| p.to_string_lossy())
                    .unwrap_or_default()
                    .as_ref(),
            ),
        ] {
            config.insert(k.to_string(), v.to_string());
        }
        Shared {
            db: Mutex::new(Keyspace::new(databases)),
            pubsub: PubSub::default(),
            requirepass,
            config: Mutex::new(config),
            scripts: Mutex::new(HashMap::new()),
            write_notify: Notify::new(),
            clients: Mutex::new(HashMap::new()),
            run_id: gen_run_id(),
            commands_processed: AtomicU64::new(0),
            connections_received: AtomicU64::new(0),
            next_client_id: AtomicU64::new(1),
            verbose: AtomicBool::new(verbose),
            dumpfile,
            last_save: AtomicU64::new(crate::db::now_ms() / 1000),
            port,
            unixsocket,
            maxclients,
            start,
        }
    }

    /// Unlink the unix socket, if there is one, on the way out. Every path that
    /// ends the process deliberately calls this; a process that dies without
    /// getting the chance leaves the file behind, which the next boot clears.
    pub fn cleanup_unixsocket(&self) {
        #[cfg(unix)]
        if let Some(path) = &self.unixsocket {
            crate::unixsocket::cleanup(path);
        }
    }

    pub fn next_client_id(&self) -> u64 {
        self.next_client_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Unix seconds of the last successful save (`LASTSAVE`).
    pub fn last_save(&self) -> u64 {
        self.last_save.load(Ordering::Relaxed)
    }

    /// Write `ks` to the configured dump file, returning `None` when no dump
    /// file was configured. Takes an already-locked keyspace because every
    /// caller either holds the lock already (`SAVE`, `SHUTDOWN`) or is the
    /// exit path, and the mutex is not reentrant.
    pub fn save_dump(&self, ks: &mut Keyspace) -> Option<Result<(), crate::rdb::Error>> {
        let path = self.dumpfile.as_ref()?;
        let result = crate::rdb::save(path, ks);
        if result.is_ok() {
            self.last_save
                .store(crate::db::now_ms() / 1000, Ordering::Relaxed);
        }
        Some(result)
    }

    /// [`Shared::save_dump`] for callers that do not already hold the lock.
    pub fn save_dump_locking(&self) -> Option<Result<(), crate::rdb::Error>> {
        let mut ks = self.db.lock().unwrap();
        self.save_dump(&mut ks)
    }

    /// Whether command logging is currently on.
    pub fn verbose(&self) -> bool {
        self.verbose.load(Ordering::Relaxed)
    }

    /// Turn command logging on or off (`CONFIG SET loglevel`).
    pub fn set_verbose(&self, on: bool) {
        self.verbose.store(on, Ordering::Relaxed);
    }
}

/// Build a 40-hex-character run id, the way Redis reports one in `INFO`.
fn gen_run_id() -> String {
    let mut s = String::with_capacity(40);
    while s.len() < 40 {
        s.push_str(&format!("{:016x}", crate::commands::rand_u64()));
    }
    s.truncate(40);
    s
}

/// Snapshot of a client, kept in the shared registry.
#[derive(Clone)]
pub struct ClientInfo {
    pub id: u64,
    pub addr: String,
    pub name: String,
    pub resp3: bool,
    /// The database this client has `SELECT`ed, mirrored here so `CLIENT LIST`
    /// can report other connections' `db=` without reaching into their state.
    pub db: usize,
}

/// State owned by a single connection's task.
pub struct ConnState {
    pub id: u64,
    /// How this client is reported by `CLIENT LIST` — `host:port` for a TCP
    /// peer, and the socket path with Redis' placeholder `:0` for a unix one.
    pub addr: String,
    pub name: Bytes,
    /// Whether the client negotiated RESP3 via `HELLO 3`.
    pub resp3: bool,
    /// Database this connection has `SELECT`ed; every data command runs against
    /// `Keyspace::db(db_index)`. Always in range — `SELECT` is the only way to
    /// change it and it range-checks.
    pub db_index: usize,
    pub authenticated: bool,
    pub subscribed_channels: HashSet<Bytes>,
    pub subscribed_patterns: HashSet<Bytes>,
    /// True between `MULTI` and `EXEC`/`DISCARD`.
    pub in_multi: bool,
    /// Commands queued during a transaction.
    pub multi_queue: Vec<Vec<Bytes>>,
    /// Set when a queued command was malformed, so `EXEC` aborts.
    pub multi_error: bool,
    /// Keys watched via `WATCH`, mapped to `(existed, fingerprint)` snapshots
    /// taken at watch time. `EXEC` aborts if any of these changed. Keyed by
    /// `(database, key)` because a watch is scoped to the database that was
    /// selected when it was taken — the same key name in another database is a
    /// different watch.
    pub watched: HashMap<(usize, Bytes), (bool, u64)>,
    /// Sender the pub/sub layer uses to push messages to this connection.
    pub tx: UnboundedSender<Frame>,
}

impl ConnState {
    /// Total number of channel + pattern subscriptions (used in reply counts).
    pub fn subscription_count(&self) -> usize {
        self.subscribed_channels.len() + self.subscribed_patterns.len()
    }
}
