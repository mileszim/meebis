//! Process-wide shared state and per-connection state.

use crate::db::Db;
use crate::pubsub::PubSub;
use crate::resp::Frame;
use bytes::Bytes;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::Notify;

/// State shared across every connection. Cheap to `Arc`-clone.
pub struct Shared {
    pub db: Mutex<Db>,
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
    pub port: u16,
    pub maxclients: usize,
    pub start: Instant,
}

impl Shared {
    pub fn new(
        requirepass: Option<String>,
        port: u16,
        maxclients: usize,
        start: Instant,
    ) -> Shared {
        let mut config = HashMap::new();
        for (k, v) in [
            ("maxmemory", "0"),
            ("maxmemory-policy", "noeviction"),
            ("save", ""),
            ("appendonly", "no"),
            ("appendfsync", "everysec"),
            ("databases", "16"),
            ("maxclients", "10000"),
            ("timeout", "0"),
            ("tcp-keepalive", "300"),
        ] {
            config.insert(k.to_string(), v.to_string());
        }
        Shared {
            db: Mutex::new(Db::new()),
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
            port,
            maxclients,
            start,
        }
    }

    pub fn next_client_id(&self) -> u64 {
        self.next_client_id.fetch_add(1, Ordering::Relaxed)
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
}

/// State owned by a single connection's task.
pub struct ConnState {
    pub id: u64,
    pub addr: SocketAddr,
    pub name: Bytes,
    /// Whether the client negotiated RESP3 via `HELLO 3`.
    pub resp3: bool,
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
    /// taken at watch time. `EXEC` aborts if any of these changed.
    pub watched: HashMap<Bytes, (bool, u64)>,
    /// Sender the pub/sub layer uses to push messages to this connection.
    pub tx: UnboundedSender<Frame>,
}

impl ConnState {
    /// Total number of channel + pattern subscriptions (used in reply counts).
    pub fn subscription_count(&self) -> usize {
        self.subscribed_channels.len() + self.subscribed_patterns.len()
    }
}
