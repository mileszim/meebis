//! `--seed` is a promise about a *file*: it is read at boot and never written,
//! renamed, or otherwise touched. These tests take that literally and compare
//! the bytes on disk before and after, including on the paths where a
//! `--dumpfile` would have moved it.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// A temp directory that removes itself however the test ends.
struct Dir(PathBuf);

impl Dir {
    fn new(tag: &str) -> Dir {
        let dir = std::env::temp_dir().join(format!("meebis-seed-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("could not make a temp dir");
        Dir(dir)
    }
    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
    /// The files in the directory, sorted — enough to prove nothing was
    /// renamed alongside the original.
    fn entries(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(&self.0)
            .expect("readable temp dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A running server, killed if a test leaves without stopping it.
struct Server {
    child: Child,
    port: u16,
    banner: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    /// Send one inline command and read the reply.
    fn ask(&self, command: &str) -> String {
        let mut sock = TcpStream::connect(("127.0.0.1", self.port)).expect("connect");
        sock.set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .expect("set a read timeout");
        sock.write_all(format!("{command}\r\n").as_bytes())
            .expect("write");

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

    /// Stop cleanly, taking whatever exit path a real client would.
    fn shutdown(mut self) {
        let mut sock = TcpStream::connect(("127.0.0.1", self.port)).expect("connect");
        let _ = sock.write_all(b"SHUTDOWN\r\n");
        self.child.wait().expect("the server should exit");
    }
}

/// Start meebis on an OS-assigned port and wait for its banner, which is only
/// printed once the listener is up and the snapshot has been read.
fn start(dir: &Dir, args: &[&str]) -> Server {
    let port_file = dir.join("port");
    let _ = std::fs::remove_file(&port_file);

    let mut child = Command::new(env!("CARGO_BIN_EXE_meebis"))
        .args(["--port", "0", "--port-file", port_file.to_str().unwrap()])
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to run the meebis binary");

    let mut banner = String::new();
    let mut out = BufReader::new(child.stdout.take().expect("piped stdout"));
    // Skip the load messages; the banner is the line that announces the port.
    loop {
        banner.clear();
        let n = out.read_line(&mut banner).expect("server output");
        assert_ne!(n, 0, "the server exited before it was ready");
        if banner.contains("ready on") {
            break;
        }
    }
    let port = std::fs::read_to_string(&port_file)
        .expect("the port file should exist once the banner is out")
        .trim()
        .parse()
        .expect("a numeric port");

    Server {
        child,
        port,
        banner,
    }
}

/// Run meebis to completion and hand back (exit code, stderr). Always on an
/// OS-assigned port: these are tests about refusing to start, and they must
/// fail for the reason under test rather than because the machine happens to
/// have something on 6379.
fn run(args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_meebis"))
        .args(["--port", "0"])
        .args(args)
        .output()
        .expect("failed to run the meebis binary");
    (
        out.status.code().expect("child was killed by a signal"),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Write a real RDB by letting meebis save one, which is also the file a user
/// would produce when building a fixture.
fn make_fixture(dir: &Dir, name: &str) -> PathBuf {
    let path = dir.join(name);
    let server = start(dir, &["--dumpfile", path.to_str().unwrap()]);
    assert_eq!(server.ask("SET fixture present"), "+OK\r\n");
    assert_eq!(server.ask("SET counter 1"), "+OK\r\n");
    server.shutdown();
    assert!(path.exists(), "the fixture should have been written");
    path
}

fn bytes(path: &Path) -> Vec<u8> {
    std::fs::read(path).expect("readable file")
}

#[test]
fn a_seed_is_loaded_and_never_written_back() {
    let dir = Dir::new("readonly");
    let seed = make_fixture(&dir, "golden.rdb");
    let before = bytes(&seed);

    let server = start(&dir, &["--seed", seed.to_str().unwrap()]);
    assert!(
        server.banner.contains("read-only"),
        "the banner should say the seed is read-only, got {:?}",
        server.banner
    );

    // The fixture is there to be used.
    assert_eq!(server.ask("GET fixture"), "$7\r\npresent\r\n");

    // Diverge from it, then take every path that would write a dump file.
    assert_eq!(server.ask("SET counter 999"), "+OK\r\n");
    assert_eq!(server.ask("SET local yes"), "+OK\r\n");
    assert_eq!(server.ask("SAVE"), "+OK\r\n");
    assert_eq!(server.ask("BGSAVE"), "+Background saving started\r\n");
    server.shutdown();

    assert_eq!(
        bytes(&seed),
        before,
        "SAVE/BGSAVE/SHUTDOWN must all leave the seed untouched"
    );
    assert_eq!(
        dir.entries(),
        vec!["golden.rdb".to_string(), "port".to_string()],
        "nothing should have been written beside the seed"
    );
}

#[test]
fn instances_sharing_a_seed_do_not_disturb_each_other() {
    let dir = Dir::new("shared");
    let seed = make_fixture(&dir, "golden.rdb");
    let before = bytes(&seed);

    // The pattern the flag exists for: several worktrees, one fixture.
    let a = start(&dir, &["--seed", seed.to_str().unwrap()]);
    let b = start(&dir, &["--seed", seed.to_str().unwrap()]);

    assert_eq!(a.ask("GET fixture"), "$7\r\npresent\r\n");
    assert_eq!(b.ask("GET fixture"), "$7\r\npresent\r\n");

    // Each diverges privately.
    assert_eq!(a.ask("SET who a"), "+OK\r\n");
    assert_eq!(b.ask("SET who b"), "+OK\r\n");
    assert_eq!(a.ask("GET who"), "$1\r\na\r\n");
    assert_eq!(b.ask("GET who"), "$1\r\nb\r\n");

    a.shutdown();
    b.shutdown();
    assert_eq!(bytes(&seed), before, "neither instance may write the seed");
}

#[test]
fn an_unreadable_seed_is_left_exactly_where_it_is() {
    let dir = Dir::new("corrupt");
    let seed = dir.join("golden.rdb");
    std::fs::write(&seed, b"REDIS0011 and then nonsense").expect("write");
    let before = bytes(&seed);

    let server = start(&dir, &["--seed", seed.to_str().unwrap()]);
    // Starting empty beats refusing to boot, exactly as for a dump file.
    assert_eq!(server.ask("DBSIZE"), ":0\r\n");
    server.shutdown();

    assert_eq!(bytes(&seed), before, "the seed's contents must survive");
    assert_eq!(
        dir.entries(),
        vec!["golden.rdb".to_string(), "port".to_string()],
        "a seed must not be renamed aside the way a dump file is"
    );
}

#[test]
fn an_unreadable_dumpfile_is_still_set_aside() {
    // The counterpart to the test above: --dumpfile's behavior is unchanged,
    // because meebis owns that file and is about to overwrite it.
    let dir = Dir::new("setaside");
    let dump = dir.join("dump.rdb");
    std::fs::write(&dump, b"REDIS0011 and then nonsense").expect("write");

    let server = start(&dir, &["--dumpfile", dump.to_str().unwrap()]);
    assert_eq!(server.ask("DBSIZE"), ":0\r\n");
    server.shutdown();

    assert!(
        dir.entries()
            .iter()
            .any(|n| n.starts_with("dump.rdb.unreadable-")),
        "the unreadable dump should have been preserved, got {:?}",
        dir.entries()
    );
}

#[test]
fn a_missing_seed_warns_but_starts() {
    let dir = Dir::new("missing");
    let seed = dir.join("nope.rdb");

    let server = start(&dir, &["--seed", seed.to_str().unwrap()]);
    assert_eq!(server.ask("PING"), "+PONG\r\n");
    server.shutdown();

    assert!(!seed.exists(), "a missing seed must not be created");
}

#[test]
fn a_missing_seed_is_fatal_under_strict() {
    let dir = Dir::new("strict");
    let (code, stderr) = run(&[
        "--seed",
        dir.join("nope.rdb").to_str().unwrap(),
        "--dumpfile-strict",
    ]);
    assert_eq!(code, 1);
    assert!(stderr.contains("no seed at"), "got {stderr:?}");
}

#[test]
fn an_unreadable_seed_is_fatal_under_strict() {
    let dir = Dir::new("strictbad");
    let seed = dir.join("golden.rdb");
    std::fs::write(&seed, b"not an rdb at all").expect("write");

    let (code, stderr) = run(&["--seed", seed.to_str().unwrap(), "--dumpfile-strict"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("could not load"), "got {stderr:?}");
    assert_eq!(
        dir.entries(),
        vec!["golden.rdb".to_string()],
        "even refusing to start must not move the seed"
    );
}

#[test]
fn seed_and_dumpfile_are_mutually_exclusive() {
    let dir = Dir::new("both");
    let seed = dir.join("golden.rdb");
    let seed = seed.to_str().unwrap();
    for other in [
        ["--dumpfile", "d.rdb"],
        ["--dir", "."],
        ["--dbfilename", "d.rdb"],
    ] {
        let args = ["--seed", seed, other[0], other[1]];
        let (code, stderr) = run(&args);
        assert_eq!(code, 1, "expected a usage error for {args:?}");
        assert!(stderr.contains("--seed"), "got {stderr:?}");
    }
}

#[test]
fn seed_works_under_run() {
    let dir = Dir::new("run");
    let seed = make_fixture(&dir, "golden.rdb");
    let before = bytes(&seed);

    // `meebis run` flushes a dump file when the command exits; a seed must sit
    // out that path too.
    let out = Command::new(env!("CARGO_BIN_EXE_meebis"))
        .args([
            "run",
            "--seed",
            seed.to_str().unwrap(),
            "--",
            "sh",
            "-c",
            "exit 0",
        ])
        .output()
        .expect("failed to run the meebis binary");
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(bytes(&seed), before, "`run` must not write the seed either");
}
