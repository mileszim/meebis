//! meebis — a fast, disposable, in-memory Redis-compatible server.
//!
//! The server itself, which the `meebis` binary wraps with its command line. Use [`spawn`] to run
//! one inside another process, such as a test suite that needs a Redis without installing one.

// These clippy lints prefer very-recent stdlib helpers (`is_multiple_of`,
// `is_none_or`) or rewrites we find no clearer than the explicit forms kept
// here; the test modules are also intentionally placed mid-file.
#![allow(
    clippy::unnecessary_map_or,
    clippy::manual_is_multiple_of,
    clippy::manual_range_contains,
    clippy::explicit_counter_loop,
    clippy::items_after_test_module,
    clippy::len_without_is_empty
)]

#[doc(hidden)]
pub mod commands;
#[doc(hidden)]
pub mod db;
#[doc(hidden)]
pub mod log;
#[doc(hidden)]
pub mod pubsub;
#[doc(hidden)]
pub mod rdb;
#[doc(hidden)]
pub mod resp;
#[doc(hidden)]
pub mod server;
#[doc(hidden)]
pub mod sha1;
#[cfg(unix)]
#[doc(hidden)]
pub mod unixsocket;

use bytes::BytesMut;
use server::{ClientInfo, ConnState, Shared};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

/// Starts a server on 127.0.0.1, on a port the OS picks, and returns its address. It runs on a
/// thread and runtime of its own, so it outlives whichever async runtime called this, until the
/// process exits. It has no password, no persistence and the default 16 databases.
pub fn spawn() -> std::io::Result<SocketAddr> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let addr = listener.local_addr()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    std::thread::Builder::new()
        .name("meebis".into())
        .spawn(move || {
            runtime.block_on(async move {
                let listener = TcpListener::from_std(listener).expect("meebis: listener");
                let shared = Arc::new(Shared::new(
                    None,
                    addr.port(),
                    None,
                    10_000,
                    16,
                    false,
                    None,
                    Instant::now(),
                ));
                tokio::spawn(sweep_expired(shared.clone()));
                accept_tcp(listener, shared).await;
            })
        })?;
    Ok(addr)
}

/// Periodically drop keys whose TTL has elapsed so memory doesn't creep.
#[doc(hidden)]
pub async fn sweep_expired(shared: Arc<Shared>) {
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    loop {
        ticker.tick().await;
        for db in shared.db.lock().unwrap().iter_mut() {
            db.sweep_expired();
        }
    }
}

/// Accept TCP connections forever.
#[doc(hidden)]
pub async fn accept_tcp(listener: TcpListener, shared: Arc<Shared>) {
    loop {
        let (stream, addr) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("meebis: accept error: {e}");
                continue;
            }
        };
        let _ = stream.set_nodelay(true);
        shared.connections_received.fetch_add(1, Ordering::Relaxed);
        let shared = shared.clone();
        tokio::spawn(async move {
            let _ = handle_connection(shared, stream, addr.to_string()).await;
        });
    }
}

/// Accept unix-socket connections forever. A unix peer has no address of its
/// own, so clients are reported by the socket path, exactly as Redis does.
#[cfg(unix)]
#[doc(hidden)]
pub async fn accept_unix(listener: tokio::net::UnixListener, shared: Arc<Shared>) {
    let addr = shared
        .unixsocket
        .as_deref()
        .map(unixsocket::peer_addr)
        .unwrap_or_default();
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("meebis: accept error: {e}");
                continue;
            }
        };
        shared.connections_received.fetch_add(1, Ordering::Relaxed);
        let shared = shared.clone();
        let addr = addr.clone();
        tokio::spawn(async move {
            let _ = handle_connection(shared, stream, addr).await;
        });
    }
}

/// Serve one client until it goes away. Generic over the transport so a TCP
/// connection and a unix-socket connection run the exact same code — the only
/// thing either half knows about the other is `addr`, the string `CLIENT LIST`
/// reports.
async fn handle_connection<S>(
    shared: Arc<Shared>,
    mut stream: S,
    addr: String,
) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let id = shared.next_client_id();

    // Enforce maxclients. The lock is released before any await below.
    let over_limit = {
        let mut clients = shared.clients.lock().unwrap();
        if clients.len() >= shared.maxclients {
            true
        } else {
            clients.insert(
                id,
                ClientInfo {
                    id,
                    addr: addr.to_string(),
                    name: String::new(),
                    resp3: false,
                    db: 0,
                },
            );
            false
        }
    };
    if over_limit {
        log::event(&shared, id, "rejected: maxclients reached");
        let mut out = BytesMut::new();
        resp::Frame::Error("ERR max number of clients reached".into()).encode(false, &mut out);
        let _ = stream.write_all(&out).await;
        return Ok(());
    }
    log::event(&shared, id, &format!("connected from {addr}"));

    let (tx, mut rx) = mpsc::unbounded_channel::<resp::Frame>();
    let mut conn = ConnState {
        id,
        addr,
        name: bytes::Bytes::new(),
        resp3: false,
        db_index: 0,
        authenticated: false,
        subscribed_channels: Default::default(),
        subscribed_patterns: Default::default(),
        in_multi: false,
        multi_queue: Vec::new(),
        multi_error: false,
        watched: HashMap::new(),
        tx,
    };

    let mut buf = BytesMut::with_capacity(16 * 1024);
    let mut close = false;

    while !close {
        tokio::select! {
            // Inbound bytes from the client.
            read = stream.read_buf(&mut buf) => {
                let n = read?;
                if n == 0 {
                    break; // client closed
                }
                let mut out = BytesMut::new();
                loop {
                    match resp::parse_command(&mut buf) {
                        Ok(Some(args)) => {
                            shared.commands_processed.fetch_add(1, Ordering::Relaxed);
                            let started = log::cmd(&shared, &conn, &args);
                            match commands::handle(&shared, &mut conn, args) {
                                commands::Reply::None => {}
                                commands::Reply::One(f) => {
                                    log::reply(&shared, &conn, &f, started);
                                    f.encode(conn.resp3, &mut out);
                                }
                                commands::Reply::Many(frames) => {
                                    log::replies(&shared, &conn, &frames, started);
                                    for f in frames {
                                        f.encode(conn.resp3, &mut out);
                                    }
                                }
                                commands::Reply::Close(f) => {
                                    log::reply(&shared, &conn, &f, started);
                                    f.encode(conn.resp3, &mut out);
                                    close = true;
                                    break;
                                }
                                commands::Reply::Block(req) => {
                                    // Flush anything queued before this
                                    // command, then park until data arrives
                                    // or the deadline passes.
                                    if !out.is_empty() {
                                        stream.write_all(&out).await?;
                                        out.clear();
                                    }
                                    log::event(&shared, conn.id, "blocked, waiting for data");
                                    let frame = block_until_ready(
                                        &shared, &mut conn, req,
                                    ).await;
                                    log::reply(&shared, &conn, &frame, started);
                                    frame.encode(conn.resp3, &mut out);
                                }
                            }
                        }
                        Ok(None) => break, // need more bytes
                        Err(resp::ParseError::Incomplete) => break,
                        Err(resp::ParseError::Protocol(msg)) => {
                            log::event(&shared, conn.id, &format!("protocol error: {msg}"));
                            resp::Frame::Error(format!("ERR Protocol error: {msg}"))
                                .encode(conn.resp3, &mut out);
                            close = true;
                            break;
                        }
                    }
                }
                if !out.is_empty() {
                    stream.write_all(&out).await?;
                }
            }
            // Out-of-band pub/sub messages destined for this client.
            Some(frame) = rx.recv() => {
                let mut out = BytesMut::new();
                log::reply(&shared, &conn, &frame, None);
                frame.encode(conn.resp3, &mut out);
                while let Ok(f) = rx.try_recv() {
                    log::reply(&shared, &conn, &f, None);
                    f.encode(conn.resp3, &mut out);
                }
                stream.write_all(&out).await?;
            }
        }
    }

    // Tear down: drop subscriptions and deregister.
    log::event(&shared, id, "disconnected");
    shared.pubsub.remove_client(id);
    shared.clients.lock().unwrap().remove(&id);
    Ok(())
}

/// Park the connection until a blocking command (`BZPOPMIN`, `XREAD BLOCK`)
/// can produce a reply, or its deadline passes.
async fn block_until_ready(
    shared: &std::sync::Arc<Shared>,
    conn: &mut ConnState,
    req: commands::BlockReq,
) -> resp::Frame {
    loop {
        // If a deadline was set, stop now if it has already passed. `None`
        // means "block forever" (BLOCK 0 / BZPOPMIN 0).
        let remaining = req.deadline_ms.map(|d| {
            let now = crate::db::now_ms();
            if now >= d {
                std::time::Duration::ZERO
            } else {
                std::time::Duration::from_millis(d - now)
            }
        });
        if matches!(remaining, Some(d) if d.is_zero()) {
            return req.timeout_reply;
        }

        // Register the notify future BEFORE polling, so a wake that arrives
        // between the poll and the await is not lost.
        let notified = shared.write_notify.notified();
        tokio::pin!(notified);

        if let Some(frame) = commands::retry_block(shared, conn, &req) {
            return frame;
        }

        match remaining {
            Some(d) => match tokio::time::timeout(d, notified).await {
                Ok(()) => continue,
                Err(_) => return req.timeout_reply,
            },
            None => {
                notified.await;
            }
        }
    }
}
