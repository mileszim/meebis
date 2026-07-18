//! meebis — a fast, disposable, in-memory Redis-compatible server.
//!
//! Boots clean, keeps everything in RAM, and forgets it all on exit. Designed
//! to be spun up per-worktree, connected to by a few processes, and thrown
//! away. Speaks enough of the RESP wire protocol and Redis command surface to
//! stand in for Redis in local development and tests.

// These clippy lints prefer very-recent stdlib helpers (`is_multiple_of`,
// `is_none_or`) or rewrites we find no clearer than the explicit forms kept
// here; the test modules are also intentionally placed mid-file.
#![allow(
    clippy::unnecessary_map_or,
    clippy::manual_is_multiple_of,
    clippy::manual_range_contains,
    clippy::explicit_counter_loop,
    clippy::items_after_test_module
)]

mod commands;
mod db;
mod pubsub;
mod resp;
mod server;

use bytes::BytesMut;
use server::{ClientInfo, ConnState, Shared};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Parsed command-line configuration.
struct Config {
    bind: String,
    port: u16,
    requirepass: Option<String>,
    maxclients: usize,
}

fn print_help() {
    println!(
        "meebis {VERSION} — a disposable, in-memory Redis-compatible server

USAGE:
    meebis [OPTIONS]

OPTIONS:
    -p, --port <PORT>          Port to listen on (default: 6379)
        --bind <ADDR>          Address to bind (default: 127.0.0.1)
        --requirepass <PASS>   Require AUTH with this password
        --maxclients <N>       Maximum simultaneous connections (default: 10000)
    -h, --help                 Print this help
    -v, --version              Print version

Everything is kept in memory and discarded on exit. There is no persistence."
    );
}

/// Parse argv. Returns `Err(exit_code)` when the process should exit early
/// (after printing help/version or on a bad argument).
fn parse_args() -> Result<Config, i32> {
    let mut cfg = Config {
        bind: "127.0.0.1".to_string(),
        port: 6379,
        requirepass: None,
        maxclients: 10000,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print_help();
                return Err(0);
            }
            "-v" | "--version" => {
                println!("meebis {VERSION}");
                return Err(0);
            }
            "-p" | "--port" => match args.next().and_then(|v| v.parse::<u16>().ok()) {
                Some(p) => cfg.port = p,
                None => {
                    eprintln!("meebis: --port requires a valid port number");
                    return Err(1);
                }
            },
            "--bind" => match args.next() {
                Some(b) => cfg.bind = b,
                None => {
                    eprintln!("meebis: --bind requires an address");
                    return Err(1);
                }
            },
            "--requirepass" => match args.next() {
                Some(p) => cfg.requirepass = Some(p),
                None => {
                    eprintln!("meebis: --requirepass requires a value");
                    return Err(1);
                }
            },
            "--maxclients" => match args.next().and_then(|v| v.parse::<usize>().ok()) {
                Some(n) => cfg.maxclients = n,
                None => {
                    eprintln!("meebis: --maxclients requires a number");
                    return Err(1);
                }
            },
            other => {
                eprintln!("meebis: unknown option '{other}' (try --help)");
                return Err(1);
            }
        }
    }
    Ok(cfg)
}

fn main() {
    let cfg = match parse_args() {
        Ok(c) => c,
        Err(code) => std::process::exit(code),
    };

    // A single-threaded runtime keeps the per-instance footprint tiny (one OS
    // thread), which matters when running dozens of these at once. Command
    // execution is serialized behind one mutex, just like Redis.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    if let Err(e) = rt.block_on(run(cfg)) {
        eprintln!("meebis: {e}");
        std::process::exit(1);
    }
}

async fn run(cfg: Config) -> std::io::Result<()> {
    let start = Instant::now();

    let bind_addr = format!("{}:{}", cfg.bind, cfg.port);
    let listener = TcpListener::bind(&bind_addr)
        .await
        .map_err(|e| std::io::Error::new(e.kind(), format!("could not bind {bind_addr}: {e}")))?;
    // Resolve the actual port (matters when --port 0 asks the OS to pick one).
    let local_addr = listener.local_addr()?;

    let shared = Arc::new(Shared::new(
        cfg.requirepass,
        local_addr.port(),
        cfg.maxclients,
        start,
    ));

    println!(
        "meebis {} ready on {} (pid {}) — in-memory, no persistence",
        VERSION,
        local_addr,
        std::process::id()
    );

    // Exit cleanly on Ctrl-C; there is nothing to flush.
    tokio::spawn(async {
        let _ = tokio::signal::ctrl_c().await;
        std::process::exit(0);
    });

    // Periodically drop keys whose TTL has elapsed so memory doesn't creep.
    tokio::spawn({
        let shared = shared.clone();
        async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(1));
            loop {
                ticker.tick().await;
                shared.db.lock().unwrap().sweep_expired();
            }
        }
    });

    loop {
        let (stream, addr) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("meebis: accept error: {e}");
                continue;
            }
        };
        shared.connections_received.fetch_add(1, Ordering::Relaxed);
        let shared = shared.clone();
        tokio::spawn(async move {
            let _ = handle_connection(shared, stream, addr).await;
        });
    }
}

async fn handle_connection(
    shared: Arc<Shared>,
    mut stream: TcpStream,
    addr: SocketAddr,
) -> std::io::Result<()> {
    let _ = stream.set_nodelay(true);
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
                },
            );
            false
        }
    };
    if over_limit {
        let mut out = BytesMut::new();
        resp::Frame::Error("ERR max number of clients reached".into()).encode(false, &mut out);
        let _ = stream.write_all(&out).await;
        return Ok(());
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<resp::Frame>();
    let mut conn = ConnState {
        id,
        addr,
        name: bytes::Bytes::new(),
        resp3: false,
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
                            match commands::handle(&shared, &mut conn, args) {
                                commands::Reply::None => {}
                                commands::Reply::One(f) => f.encode(conn.resp3, &mut out),
                                commands::Reply::Many(frames) => {
                                    for f in frames {
                                        f.encode(conn.resp3, &mut out);
                                    }
                                }
                                commands::Reply::Close(f) => {
                                    f.encode(conn.resp3, &mut out);
                                    close = true;
                                    break;
                                }
                            }
                        }
                        Ok(None) => break, // need more bytes
                        Err(resp::ParseError::Incomplete) => break,
                        Err(resp::ParseError::Protocol(msg)) => {
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
                frame.encode(conn.resp3, &mut out);
                while let Ok(f) = rx.try_recv() {
                    f.encode(conn.resp3, &mut out);
                }
                stream.write_all(&out).await?;
            }
        }
    }

    // Tear down: drop subscriptions and deregister.
    shared.pubsub.remove_client(id);
    shared.clients.lock().unwrap().remove(&id);
    Ok(())
}
