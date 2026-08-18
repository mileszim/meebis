//! `--unixsocket` is about *addressing*, not about the command surface, so
//! these tests are about what listens where: that a socket answers RESP, that
//! asking for one takes the TCP port away, that an explicit port brings it
//! back, and that the file does not outlive the server.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// A temp directory that removes itself, so a failed test leaves no sockets
/// lying around in `/tmp`.
struct Dir(PathBuf);

impl Dir {
    fn new(tag: &str) -> Dir {
        let dir = std::env::temp_dir().join(format!("meebis-sock-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("could not make a temp dir");
        Dir(dir)
    }
    fn sock(&self) -> PathBuf {
        self.0.join("redis.sock")
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A server that is killed when the test ends, however the test ends.
struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Start meebis and wait for the banner, which it only prints once every
/// listener is bound — so there is nothing to poll for afterwards.
fn start(args: &[&str]) -> (Server, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_meebis"))
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to run the meebis binary");

    let mut banner = String::new();
    BufReader::new(child.stdout.take().expect("piped stdout"))
        .read_line(&mut banner)
        .expect("the server should announce itself");
    (Server(child), banner)
}

/// Send one inline command over the socket and read what comes back.
fn ask(path: &Path, command: &str) -> String {
    let mut sock = UnixStream::connect(path).expect("the socket should be accepting connections");
    sock.write_all(format!("{command}\r\n").as_bytes())
        .expect("write");
    sock.set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .expect("set a read timeout");

    // Read until the server stops sending; the reply to a single command is
    // small enough to arrive well inside the timeout.
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        match sock.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.ends_with(b"\r\n") {
                    break;
                }
            }
            Err(e) => panic!("read failed: {e}"),
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

#[test]
fn a_socket_speaks_resp() {
    let dir = Dir::new("resp");
    let sock = dir.sock();
    let (_server, banner) = start(&["--unixsocket", sock.to_str().unwrap()]);

    assert!(
        banner.contains(sock.to_str().unwrap()),
        "the banner should name the socket, got {banner:?}"
    );
    assert_eq!(ask(&sock, "PING"), "+PONG\r\n");
    assert_eq!(ask(&sock, "SET hello world"), "+OK\r\n");
    assert_eq!(ask(&sock, "GET hello"), "$5\r\nworld\r\n");
}

#[test]
fn the_socket_replaces_the_port() {
    let dir = Dir::new("noport");
    let sock = dir.sock();
    let (_server, banner) = start(&["--unixsocket", sock.to_str().unwrap()]);

    // The banner is the server's own account of what it bound. Asking for a
    // socket and nothing else should not have produced a TCP address — and
    // checking the banner rather than probing 6379 keeps this from depending on
    // whether the machine already has something on that port.
    assert!(
        !banner.contains("127.0.0.1:"),
        "a lone --unixsocket should not bind TCP, got {banner:?}"
    );
    assert_eq!(ask(&sock, "PING"), "+PONG\r\n");
}

#[test]
fn an_explicit_port_listens_on_both() {
    let dir = Dir::new("both");
    let sock = dir.sock();
    let port_file = dir.0.join("port");
    let (_server, banner) = start(&[
        "--unixsocket",
        sock.to_str().unwrap(),
        "--port",
        "0",
        "--port-file",
        port_file.to_str().unwrap(),
    ]);
    assert!(
        banner.contains(" and "),
        "the banner should name both addresses, got {banner:?}"
    );

    // The socket half.
    assert_eq!(ask(&sock, "SET shared yes"), "+OK\r\n");

    // ...and the TCP half, which must reach the same keyspace.
    let port: u16 = std::fs::read_to_string(&port_file)
        .expect("the port file should exist")
        .trim()
        .parse()
        .expect("a numeric port");
    let mut tcp = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect over TCP");
    tcp.write_all(b"GET shared\r\n").expect("write");
    let mut buf = [0u8; 9];
    tcp.read_exact(&mut buf).expect("read");
    assert_eq!(&buf, b"$3\r\nyes\r\n");
}

#[test]
fn client_list_reports_the_socket_path() {
    let dir = Dir::new("clientlist");
    let sock = dir.sock();
    let (_server, _) = start(&["--unixsocket", sock.to_str().unwrap()]);

    let info = ask(&sock, "CLIENT INFO");
    let expected = format!("addr={}:0", sock.display());
    assert!(
        info.contains(&expected),
        "expected {expected:?} in {info:?}"
    );
    // Redis reports a unix client's local address as the socket too.
    assert!(
        info.contains(&format!("laddr={}:0", sock.display())),
        "laddr should be the socket, got {info:?}"
    );
}

#[test]
fn a_clean_exit_removes_the_socket() {
    let dir = Dir::new("cleanup");
    let sock = dir.sock();
    let (server, _) = start(&["--unixsocket", sock.to_str().unwrap()]);
    assert!(sock.exists(), "the socket should exist while running");

    // SHUTDOWN is the one stop path a test can take without signals.
    let mut conn = UnixStream::connect(&sock).expect("connect");
    conn.write_all(b"SHUTDOWN\r\n").expect("write");
    let mut server = server;
    server.0.wait().expect("the server should exit");

    assert!(
        !sock.exists(),
        "a clean exit should unlink the socket it created"
    );
}

#[test]
fn a_stale_socket_does_not_block_the_next_boot() {
    let dir = Dir::new("stale");
    let sock = dir.sock();

    // A server that dies without unlinking — the `kill -9` case. `Server`'s
    // drop kills without a signal handler running, which is exactly the state
    // we want to leave behind.
    {
        let (_server, _) = start(&["--unixsocket", sock.to_str().unwrap()]);
    }
    assert!(sock.exists(), "the killed server should leave the file");

    let (_server, _) = start(&["--unixsocket", sock.to_str().unwrap()]);
    assert_eq!(ask(&sock, "PING"), "+PONG\r\n");
}

#[test]
fn a_second_server_will_not_steal_a_live_socket() {
    let dir = Dir::new("contested");
    let sock = dir.sock();
    let (_first, _) = start(&["--unixsocket", sock.to_str().unwrap()]);

    let out = Command::new(env!("CARGO_BIN_EXE_meebis"))
        .args(["--unixsocket", sock.to_str().unwrap()])
        .output()
        .expect("failed to run the meebis binary");
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("already listening"),
        "unhelpful message: {stderr:?}"
    );

    // The first server is untouched by the attempt.
    assert_eq!(ask(&sock, "PING"), "+PONG\r\n");
}

#[test]
fn run_hands_the_socket_to_the_command() {
    let dir = Dir::new("run");
    let sock = dir.sock();
    let out = Command::new(env!("CARGO_BIN_EXE_meebis"))
        .args([
            "run",
            "--unixsocket",
            sock.to_str().unwrap(),
            "--",
            "sh",
            "-c",
            // REDIS_HOST/REDIS_PORT are deliberately absent here: there is no
            // host or port to point at, and a stale inherited pair would be
            // worse than nothing.
            "echo \"$REDIS_URL|$REDIS_SOCKET|${REDIS_HOST-unset}|${REDIS_PORT-unset}\"",
        ])
        .env("REDIS_HOST", "stale.example")
        .env("REDIS_PORT", "1234")
        .output()
        .expect("failed to run the meebis binary");

    assert_eq!(out.status.code(), Some(0));
    let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let parts: Vec<&str> = line.split('|').collect();
    assert_eq!(parts.len(), 4, "unexpected output {line:?}");

    // The path is made absolute for the child, which may run elsewhere.
    let canonical = std::fs::canonicalize(&dir.0)
        .expect("canonicalize")
        .join("redis.sock");
    assert_eq!(parts[0], format!("unix://{}", canonical.display()));
    assert_eq!(parts[1], canonical.to_str().unwrap());
    assert_eq!(parts[2], "unset", "a stale REDIS_HOST should be removed");
    assert_eq!(parts[3], "unset", "a stale REDIS_PORT should be removed");
}

#[test]
fn port_file_without_a_port_is_a_usage_error() {
    let dir = Dir::new("portfile");
    let out = Command::new(env!("CARGO_BIN_EXE_meebis"))
        .args([
            "--unixsocket",
            dir.sock().to_str().unwrap(),
            "--port-file",
            dir.0.join("port").to_str().unwrap(),
        ])
        .output()
        .expect("failed to run the meebis binary");

    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--port-file"), "got {stderr:?}");
}
