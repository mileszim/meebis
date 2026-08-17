//! `meebis run` is a process wrapper, so its contract is about exit codes, the
//! environment it hands down, and cleaning up after itself — not about the RESP
//! surface, which `tests/compat/` already covers against a real Redis.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};

fn meebis() -> Command {
    Command::new(env!("CARGO_BIN_EXE_meebis"))
}

/// Run to completion and hand back (exit code, stdout, stderr).
fn run(args: &[&str]) -> (i32, String, String) {
    let out = meebis()
        .args(args)
        .output()
        .expect("failed to run the meebis binary");
    (
        out.status.code().expect("child was killed by a signal"),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn forwards_the_commands_exit_code() {
    assert_eq!(run(&["run", "--", "sh", "-c", "exit 0"]).0, 0);
    assert_eq!(run(&["run", "--", "sh", "-c", "exit 42"]).0, 42);
    assert_eq!(run(&["run", "--", "sh", "-c", "exit 1"]).0, 1);
}

#[test]
fn exports_the_connection_details() {
    let (code, stdout, _) = run(&[
        "run",
        "--",
        "sh",
        "-c",
        "echo \"$REDIS_URL|$REDIS_HOST|$REDIS_PORT\"",
    ]);
    assert_eq!(code, 0);

    let line = stdout.trim();
    let parts: Vec<&str> = line.split('|').collect();
    assert_eq!(parts.len(), 3, "unexpected output {line:?}");
    let (url, host, port) = (parts[0], parts[1], parts[2]);

    assert_eq!(host, "127.0.0.1");
    let port: u16 = port.parse().expect("REDIS_PORT should be a number");
    assert_ne!(port, 0, "the resolved port should be reported, not 0");
    assert_eq!(url, format!("redis://127.0.0.1:{port}"));
}

#[test]
fn the_command_can_actually_talk_to_the_server() {
    // The child announces its port and then holds the instance open long enough
    // for us to speak RESP to it directly.
    let mut child = meebis()
        .args(["run", "--", "sh", "-c", "echo $REDIS_PORT; sleep 5"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn");

    let mut stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
    let mut line = String::new();
    stdout
        .read_line(&mut line)
        .expect("child should report a port");
    let port: u16 = line.trim().parse().expect("a numeric port");

    let mut sock = std::net::TcpStream::connect(("127.0.0.1", port))
        .expect("the server should be accepting connections before the command runs");
    sock.write_all(b"PING\r\n").expect("write PING");
    let mut buf = [0u8; 7];
    sock.read_exact(&mut buf).expect("read the reply");
    assert_eq!(&buf, b"+PONG\r\n");

    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn concurrent_instances_get_different_ports() {
    let spawn_one = || {
        meebis()
            .args(["run", "--", "sh", "-c", "echo $REDIS_PORT; sleep 2"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn")
    };
    let (mut a, mut b) = (spawn_one(), spawn_one());

    let port_of = |child: &mut std::process::Child| -> String {
        let mut line = String::new();
        BufReader::new(child.stdout.take().expect("piped stdout"))
            .read_line(&mut line)
            .expect("a port line");
        line.trim().to_string()
    };
    // Both are bound at the same time, so the OS cannot have handed out one
    // port twice — this is the property that lets several run side by side.
    let (pa, pb) = (port_of(&mut a), port_of(&mut b));
    assert_ne!(pa, pb, "both instances bound port {pa}");

    for mut child in [a, b] {
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[test]
fn extra_env_names_also_receive_the_url() {
    let (code, stdout, _) = run(&[
        "run",
        "--env",
        "CACHE_URL",
        "--env",
        "SIDEKIQ_REDIS_URL",
        "--",
        "sh",
        "-c",
        "test \"$CACHE_URL\" = \"$REDIS_URL\" && test \"$SIDEKIQ_REDIS_URL\" = \"$REDIS_URL\" && echo same",
    ]);
    assert_eq!(code, 0, "stdout: {stdout}");
    assert_eq!(stdout.trim(), "same");
}

#[test]
fn the_command_keeps_stdout_to_itself() {
    // meebis' banner and trace go to stderr under `run`, so a caller can
    // redirect stdout and capture only what the command wrote.
    let (code, stdout, stderr) = run(&["run", "--", "sh", "-c", "echo ONLY_THIS"]);
    assert_eq!(code, 0);
    assert_eq!(stdout, "ONLY_THIS\n");
    assert!(
        stderr.contains("ready on"),
        "the banner should still be on stderr, got {stderr:?}"
    );
}

#[test]
fn a_missing_command_is_a_usage_error() {
    let (code, _, stderr) = run(&["run"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("--"), "unhelpful message: {stderr:?}");

    let (code, _, stderr) = run(&["run", "--"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("command"), "unhelpful message: {stderr:?}");
}

#[test]
fn a_command_that_does_not_exist_exits_127() {
    let (code, _, stderr) = run(&["run", "--", "meebis-no-such-command"]);
    assert_eq!(code, 127, "the shell's convention for command-not-found");
    assert!(stderr.contains("meebis-no-such-command"));
}

#[test]
fn env_is_rejected_outside_run() {
    let (code, _, stderr) = run(&["--env", "CACHE_URL"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("meebis run"), "got {stderr:?}");
}

#[test]
fn options_before_the_separator_still_apply() {
    // Everything after `--` belongs to the command, including things that look
    // like meebis' own flags.
    let (code, stdout, _) = run(&[
        "run",
        "--requirepass",
        "hunter2",
        "--",
        "sh",
        "-c",
        "echo $REDIS_URL",
    ]);
    assert_eq!(code, 0);
    assert!(
        stdout.trim().starts_with("redis://:hunter2@127.0.0.1:"),
        "got {stdout:?}"
    );

    let (code, stdout, _) = run(&["run", "--", "echo", "--verbose"]);
    assert_eq!(code, 0);
    assert_eq!(stdout.trim(), "--verbose");
}
