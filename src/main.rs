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
mod log;
mod pubsub;
mod rdb;
mod resp;
mod runner;
mod server;
mod sha1;
#[cfg(unix)]
mod unixsocket;

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

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Parsed command-line configuration.
struct Config {
    bind: String,
    port: u16,
    /// Whether to bind a TCP listener at all. Always true unless a unix socket
    /// was asked for without an explicit port — see [`parse_args`].
    tcp: bool,
    /// Unix-domain socket to listen on, alongside or instead of TCP.
    unixsocket: Option<std::path::PathBuf>,
    port_file: Option<String>,
    requirepass: Option<String>,
    maxclients: usize,
    /// Number of `SELECT`able databases (Redis' `databases` config).
    databases: usize,
    /// Log every command and reply (`--verbose`, `--loglevel verbose|debug`).
    verbose: bool,
    /// RDB snapshot to load at boot and write at exit. `None` keeps meebis
    /// purely ephemeral, which is still the default.
    dumpfile: Option<std::path::PathBuf>,
    /// Refuse to start when the dump exists but cannot be loaded, instead of
    /// setting it aside and starting empty.
    dumpfile_strict: bool,
    /// `meebis run -- <command>`: the command to run against this instance,
    /// which then lives exactly as long as the command does.
    run: Option<runner::Spec>,
}

fn print_help() {
    println!(
        "meebis {VERSION} — a disposable, in-memory Redis-compatible server

USAGE:
    meebis [OPTIONS]
    meebis run [OPTIONS] -- <COMMAND> [ARGS...]

RUN:
    `meebis run` starts a server, runs <COMMAND> against it, and shuts the
    server down when the command exits — exiting with the command's own status.
    The command is handed its connection details in the environment:

        REDIS_URL    redis://127.0.0.1:<port>   (with the password, if any)
        REDIS_HOST   the address to dial
        REDIS_PORT   the port that was bound

    Without an explicit --port the OS picks a free one, so any number of these
    can run at once — one per worktree, or several in the same CI job — with no
    port collisions and no cleanup:

        meebis run -- npm test
        meebis run --requirepass hunter2 -- ./bin/rails test
        meebis run --env CACHE_URL -- pytest

    With --unixsocket there is no port at all, so REDIS_URL becomes
    unix://<path>, REDIS_SOCKET holds the same path, and REDIS_HOST/REDIS_PORT
    are unset rather than left pointing somewhere stale:

        meebis run --unixsocket .meebis/redis.sock -- npm test

    Options for the server go before the `--`; everything after it belongs to
    the command. meebis' own output goes to stderr in this mode, so the
    command keeps stdout to itself.

OPTIONS:
        --env <NAME>           (run only, repeatable) also set <NAME> to the
                               connection URL, for apps that read something
                               other than REDIS_URL
    -p, --port <PORT>          Port to listen on (default: 6379)
        --bind <ADDR>          Address to bind (default: 127.0.0.1)
        --unixsocket <PATH>    Listen on a unix-domain socket at <PATH>. On its
                               own this replaces the TCP port entirely, so the
                               path is the whole address and nothing has to be
                               allocated or discovered; add an explicit --port
                               to listen on both
        --port-file <PATH>     Write the actual listen port to <PATH> on boot
                               (useful with --port 0, so tooling can find it)
        --requirepass <PASS>   Require AUTH with this password
        --maxclients <N>       Maximum simultaneous connections (default: 10000)
        --databases <N>        Number of SELECTable databases (default: 16)
        --dumpfile <PATH>      Load this RDB snapshot at boot and write it back
                               on exit, on SHUTDOWN, and on SAVE/BGSAVE
        --dir <DIR>            Redis' spelling of the same thing: the snapshot
        --dbfilename <NAME>    is <DIR>/<NAME> (default dir '.', name dump.rdb)
        --dumpfile-strict      Exit rather than start empty when a dump exists
                               but cannot be loaded
        --verbose              Log every command and reply to stdout
        --loglevel <LEVEL>     nothing|warning|notice|verbose|debug
                               (default: notice; verbose and debug log every
                               command, same as --verbose)
    -h, --help                 Print this help
    -v, --version              Print version

Without a dump file, everything is kept in memory and discarded on exit.

A dump file does not make meebis durable — the keyspace still lives only in
RAM, and only a clean exit writes it out. What it buys is handing state across:
seed an instance from a snapshot a real Redis wrote, or keep a worktree's
keyspace across a restart. The file is Redis' own RDB format, so redis-server
can read what meebis writes and vice versa.

Verbose logging can also be toggled on a running server:

    redis-cli CONFIG SET loglevel verbose
    redis-cli CONFIG SET loglevel notice"
    );
}

/// Parse argv. Returns `Err(exit_code)` when the process should exit early
/// (after printing help/version or on a bad argument).
fn parse_args() -> Result<Config, i32> {
    let mut cfg = Config {
        bind: "127.0.0.1".to_string(),
        port: 6379,
        tcp: true,
        unixsocket: None,
        port_file: None,
        requirepass: None,
        maxclients: 10000,
        databases: db::DEFAULT_DATABASES,
        verbose: false,
        dumpfile: None,
        dumpfile_strict: false,
        run: None,
    };
    // `--dir`/`--dbfilename` are Redis' two-part spelling of `--dumpfile`;
    // collected separately and combined once every argument has been seen, so
    // the two can appear in either order.
    let mut dir: Option<String> = None;
    let mut dbfilename: Option<String> = None;
    let mut args = std::env::args().skip(1).peekable();

    // `run` is the one subcommand, and only valid as the first word. Its
    // presence changes two defaults: the port becomes OS-assigned (so parallel
    // invocations cannot collide) and `--` ends meebis' own options.
    let run_mode = args.peek().map(|a| a == "run").unwrap_or(false);
    if run_mode {
        args.next();
    }
    let mut port_explicit = false;
    let mut extra_env: Vec<String> = Vec::new();
    let mut child: Option<(String, Vec<String>)> = None;

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
                Some(p) => {
                    cfg.port = p;
                    port_explicit = true;
                }
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
            "--unixsocket" => match args.next() {
                Some(p) => {
                    #[cfg(unix)]
                    {
                        cfg.unixsocket = Some(p.into());
                    }
                    #[cfg(not(unix))]
                    {
                        let _ = p;
                        eprintln!("meebis: --unixsocket is not supported on this platform");
                        return Err(1);
                    }
                }
                None => {
                    eprintln!("meebis: --unixsocket requires a path");
                    return Err(1);
                }
            },
            "--port-file" => match args.next() {
                Some(p) => cfg.port_file = Some(p),
                None => {
                    eprintln!("meebis: --port-file requires a path");
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
            // Capped well above any plausible use: empty databases cost a map
            // header each, but an unbounded value would still let a typo ask
            // for gigabytes of them.
            "--databases" => match args.next().and_then(|v| v.parse::<usize>().ok()) {
                Some(n) if n >= 1 && n <= 16384 => cfg.databases = n,
                _ => {
                    eprintln!("meebis: --databases requires a number between 1 and 16384");
                    return Err(1);
                }
            },
            "--dumpfile" => match args.next() {
                Some(p) => cfg.dumpfile = Some(p.into()),
                None => {
                    eprintln!("meebis: --dumpfile requires a path");
                    return Err(1);
                }
            },
            "--dir" => match args.next() {
                Some(d) => dir = Some(d),
                None => {
                    eprintln!("meebis: --dir requires a directory");
                    return Err(1);
                }
            },
            "--dbfilename" => match args.next() {
                Some(n) => dbfilename = Some(n),
                None => {
                    eprintln!("meebis: --dbfilename requires a filename");
                    return Err(1);
                }
            },
            "--dumpfile-strict" => cfg.dumpfile_strict = true,
            "--verbose" => cfg.verbose = true,
            "--loglevel" => match args.next() {
                Some(level) => match log::level_is_verbose(&level) {
                    Some(v) => cfg.verbose = v,
                    None => {
                        eprintln!(
                            "meebis: unknown --loglevel '{level}' \
                             (nothing|warning|notice|verbose|debug)"
                        );
                        return Err(1);
                    }
                },
                None => {
                    eprintln!("meebis: --loglevel requires a level");
                    return Err(1);
                }
            },
            "--env" if run_mode => match args.next() {
                Some(name) => extra_env.push(name),
                None => {
                    eprintln!("meebis: --env requires a variable name");
                    return Err(1);
                }
            },
            "--env" => {
                eprintln!("meebis: --env is only meaningful with `meebis run`");
                return Err(1);
            }
            // Everything past `--` is the command, including anything that
            // looks like one of our own flags.
            "--" if run_mode => {
                match args.next() {
                    Some(command) => child = Some((command, args.by_ref().collect())),
                    None => {
                        eprintln!("meebis: run: `--` must be followed by a command to run");
                        return Err(1);
                    }
                }
                break;
            }
            other => {
                eprintln!("meebis: unknown option '{other}' (try --help)");
                return Err(1);
            }
        }
    }

    if run_mode {
        match child {
            Some((command, args)) => {
                cfg.run = Some(runner::Spec {
                    command,
                    args,
                    extra_env,
                })
            }
            None => {
                eprintln!(
                    "meebis: run: expected `meebis run [options] -- <command> [args...]`\n\
                     meebis: run: for example, `meebis run -- npm test`"
                );
                return Err(1);
            }
        }
        // The point of `run` is that several can run at once, so default to an
        // OS-assigned port rather than to 6379, which would collide.
        if !port_explicit {
            cfg.port = 0;
        }
    }

    // A socket path is a complete address on its own. Binding a port next to it
    // anyway would reintroduce exactly the collision the socket was chosen to
    // avoid — so `--unixsocket` alone means the socket is the whole address,
    // and asking for a port explicitly is how you say you want both.
    cfg.tcp = cfg.unixsocket.is_none() || port_explicit;
    if !cfg.tcp && cfg.port_file.is_some() {
        eprintln!(
            "meebis: --port-file has nothing to write without a TCP port \
             (pass --port too, or drop --port-file)"
        );
        return Err(1);
    }

    // An explicit --dumpfile wins; otherwise either half of Redis' spelling is
    // enough to turn persistence on, defaulting the other half the way Redis
    // does. Passing neither leaves meebis purely ephemeral.
    if cfg.dumpfile.is_none() && (dir.is_some() || dbfilename.is_some()) {
        let dir = dir.unwrap_or_else(|| ".".to_string());
        let name = dbfilename.unwrap_or_else(|| "dump.rdb".to_string());
        cfg.dumpfile = Some(std::path::Path::new(&dir).join(name));
    } else if cfg.dumpfile.is_some() && (dir.is_some() || dbfilename.is_some()) {
        eprintln!("meebis: --dumpfile cannot be combined with --dir/--dbfilename");
        return Err(1);
    }

    Ok(cfg)
}

/// Seed the keyspace from the dump before the listener opens, so no client can
/// observe the empty pre-load state.
///
/// A missing file is the normal first boot. A file that exists but will not
/// load is the interesting case: by default meebis moves it aside and starts
/// empty, because a dev server that refuses to boot is worse than one that
/// starts fresh — but it must not later overwrite the evidence, hence the
/// rename rather than a warning alone. `--dumpfile-strict` opts into Redis'
/// behavior of refusing to start.
fn load_dump(shared: &Shared, path: &std::path::Path, strict: bool) -> Result<(), i32> {
    let mut ks = shared.db.lock().unwrap();
    match rdb::load(path, &mut ks) {
        Ok(None) => {
            crate::out!("meebis: no dump at {} yet — starting empty", path.display());
        }
        Ok(Some(stats)) => {
            let from = stats
                .writer_version()
                .map(|v| format!(" (written by {v})"))
                .unwrap_or_default();
            crate::out!(
                "meebis: loaded {} key(s) from {}{}{}",
                stats.keys,
                path.display(),
                from,
                if stats.expired > 0 {
                    format!(", {} already expired", stats.expired)
                } else {
                    String::new()
                }
            );
            for loss in stats.losses() {
                eprintln!("meebis: warning: {loss}");
            }
        }
        Err(e) => {
            if strict {
                eprintln!("meebis: could not load {}: {e}", path.display());
                return Err(1);
            }
            eprintln!("meebis: could not load {}: {e}", path.display());
            match set_aside(path) {
                Ok(kept) => eprintln!(
                    "meebis: starting with an empty keyspace; the unreadable file is kept at {}",
                    kept.display()
                ),
                Err(e) => eprintln!(
                    "meebis: starting with an empty keyspace, but could not preserve the \
                     unreadable file ({e}) — it will be overwritten on the next save"
                ),
            }
        }
    }
    Ok(())
}

/// Rename an unreadable dump out of the way so the next save cannot destroy it.
fn set_aside(path: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".unreadable-{}", std::process::id()));
    let kept = path.with_file_name(name);
    std::fs::rename(path, &kept)?;
    Ok(kept)
}

/// Write the resolved listen `port` to `path` so other processes can discover
/// it — mainly useful with `--port 0`, where the OS picks the port. Written via
/// a temp file + rename so a concurrent reader never sees a half-written value;
/// (over)written fresh on each boot, so a stale file from a prior run is
/// replaced rather than trusted.
fn write_port_file(path: &str, port: u16) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = format!("{path}.tmp");
    let mut f = std::fs::File::create(&tmp)?;
    writeln!(f, "{port}")?;
    std::fs::rename(&tmp, path)
}

fn main() {
    let cfg = match parse_args() {
        Ok(c) => c,
        Err(code) => std::process::exit(code),
    };

    // Under `run` the wrapped command owns stdout; everything meebis has to say
    // goes to stderr so the two do not interleave.
    if cfg.run.is_some() {
        log::use_stderr();
    }

    // A single-threaded runtime keeps the per-instance footprint tiny (one OS
    // thread), which matters when running dozens of these at once. Command
    // execution is serialized behind one mutex, just like Redis.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    if let Err(e) = rt.block_on(serve(cfg)) {
        eprintln!("meebis: {e}");
        std::process::exit(1);
    }
}

async fn serve(cfg: Config) -> std::io::Result<()> {
    let start = Instant::now();

    // Both listeners are opened before anything else happens, so a bind failure
    // is reported before the keyspace is loaded and no half-started server ever
    // becomes visible.
    let tcp = match cfg.tcp {
        true => {
            let bind_addr = format!("{}:{}", cfg.bind, cfg.port);
            Some(TcpListener::bind(&bind_addr).await.map_err(|e| {
                std::io::Error::new(e.kind(), format!("could not bind {bind_addr}: {e}"))
            })?)
        }
        false => None,
    };
    // Resolve the actual port (matters when --port 0 asks the OS to pick one).
    // With TCP off there is no port, which is also what Redis reports.
    let local_addr = tcp.as_ref().map(|l| l.local_addr()).transpose()?;
    let port = local_addr.map(|a| a.port()).unwrap_or(0);

    #[cfg(unix)]
    let unix = match &cfg.unixsocket {
        Some(path) => Some(unixsocket::bind(path).map_err(|e| {
            std::io::Error::new(e.kind(), format!("could not bind {}: {e}", path.display()))
        })?),
        None => None,
    };

    // Publish the bound port for tooling to discover. A failure here is not
    // fatal — the server still works — but warn so a broken integration is
    // visible rather than silently hanging on a missing file.
    if let Some(path) = &cfg.port_file {
        if let Err(e) = write_port_file(path, port) {
            eprintln!("meebis: could not write --port-file {path}: {e}");
        }
    }

    // Kept for the child's REDIS_URL; the original moves into `Shared`.
    let password = cfg.requirepass.clone();

    let shared = Arc::new(Shared::new(
        cfg.requirepass,
        port,
        cfg.unixsocket.clone(),
        cfg.maxclients,
        cfg.databases,
        cfg.verbose,
        cfg.dumpfile.clone(),
        start,
    ));

    if let Some(path) = &cfg.dumpfile {
        if let Err(code) = load_dump(&shared, path, cfg.dumpfile_strict) {
            std::process::exit(code);
        }
    }

    crate::out!(
        "meebis {} ready on {} (pid {}) — {}",
        VERSION,
        listening_on(&local_addr, &cfg.unixsocket),
        std::process::id(),
        match &cfg.dumpfile {
            Some(p) => format!("in-memory, snapshotting to {}", p.display()),
            None => "in-memory, no persistence".to_string(),
        }
    );
    if cfg.verbose {
        log::note("verbose logging on — every command and reply is logged");
    }

    // Periodically drop keys whose TTL has elapsed so memory doesn't creep.
    tokio::spawn({
        let shared = shared.clone();
        async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(1));
            loop {
                ticker.tick().await;
                for db in shared.db.lock().unwrap().iter_mut() {
                    db.sweep_expired();
                }
            }
        }
    });

    // Each listener runs as its own task, so a server can answer on a port and
    // a socket at the same time without either starving the other.
    if let Some(listener) = tcp {
        tokio::spawn(accept_tcp(listener, shared.clone()));
    }
    #[cfg(unix)]
    if let Some(listener) = unix {
        tokio::spawn(accept_unix(listener, shared.clone()));
    }

    let Some(spec) = cfg.run else {
        // Snapshot and exit on Ctrl-C and on SIGTERM (how a container or a
        // supervisor stops us). Without a dump file there is nothing to flush
        // and this is the same immediate exit as before.
        spawn_signal_handler(shared.clone());
        // The listeners are running on their own tasks now; this one just has
        // to stay out of the way until a signal or `SHUTDOWN` ends the process.
        std::future::pending::<()>().await;
        unreachable!("meebis exits via a signal or SHUTDOWN, not by returning");
    };

    // `run` mode: this instance exists for exactly one command. Signals go to
    // the supervisor instead of `spawn_signal_handler`, because the child needs
    // its own chance to shut down — exiting out from under it would strand it.
    crate::out!("meebis: running: {}", spec.display());
    let endpoint = match &cfg.unixsocket {
        // Without a port there is nothing to dial but the socket. Its path is
        // made absolute because the child may well run somewhere else.
        Some(path) if !cfg.tcp => runner::Endpoint::Unix {
            path: std::fs::canonicalize(path).unwrap_or_else(|_| path.clone()),
        },
        _ => runner::Endpoint::Tcp {
            host: runner::connect_host(&cfg.bind).to_string(),
            port,
        },
    };
    let child = match runner::spawn(&spec, &endpoint, password.as_deref()) {
        Ok(child) => child,
        Err(e) => {
            eprintln!("meebis: run: could not start `{}`: {e}", spec.command);
            // The shell's "command not found", which is what this nearly
            // always is.
            std::process::exit(127);
        }
    };

    let status = runner::supervise(child).await;

    // The command is finished, so the instance has done its job. Flush the
    // snapshot exactly as a clean shutdown would, then take on the command's
    // exit code so `meebis run -- <cmd>` is transparent to whatever called it.
    if let Some(result) = shared.save_dump_locking() {
        match result {
            Ok(()) => log::note("keyspace saved"),
            Err(e) => eprintln!("meebis: could not save: {e}"),
        }
    }
    shared.cleanup_unixsocket();
    std::process::exit(status);
}

/// The addresses the banner reports, in the order a reader would look for them.
fn listening_on(tcp: &Option<SocketAddr>, unix: &Option<std::path::PathBuf>) -> String {
    match (tcp, unix) {
        (Some(addr), Some(path)) => format!("{addr} and {}", path.display()),
        (Some(addr), None) => addr.to_string(),
        (None, Some(path)) => path.display().to_string(),
        // `--unixsocket` is the only way to turn TCP off, so one of the two is
        // always present.
        (None, None) => "nothing".to_string(),
    }
}

/// Accept TCP connections forever.
async fn accept_tcp(listener: TcpListener, shared: Arc<Shared>) {
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
async fn accept_unix(listener: tokio::net::UnixListener, shared: Arc<Shared>) {
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

/// Write the snapshot (if configured) and exit. Failing to save is reported and
/// still exits: the signal already told us to go, and hanging around after a
/// SIGTERM would be worse than losing the snapshot.
fn save_and_exit(shared: &Shared, signal: &str) -> ! {
    if let Some(result) = shared.save_dump_locking() {
        match result {
            Ok(()) => log::note(&format!("{signal}: keyspace saved")),
            Err(e) => eprintln!("meebis: {signal}: could not save: {e}"),
        }
    }
    shared.cleanup_unixsocket();
    std::process::exit(0);
}

/// Wire up the signals that mean "stop". SIGTERM only exists on unix; on other
/// platforms Ctrl-C is the whole story.
fn spawn_signal_handler(shared: Arc<Shared>) {
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let mut term = match signal(SignalKind::terminate()) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("meebis: could not listen for SIGTERM: {e}");
                    let _ = tokio::signal::ctrl_c().await;
                    save_and_exit(&shared, "interrupted");
                }
            };
            tokio::select! {
                _ = tokio::signal::ctrl_c() => save_and_exit(&shared, "interrupted"),
                _ = term.recv() => save_and_exit(&shared, "terminated"),
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
            save_and_exit(&shared, "interrupted");
        }
    });
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
