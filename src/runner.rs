//! `meebis run -- <command>`: run one command against a private instance.
//!
//! The server binds a port, hands the child process its connection details in
//! the environment, and lives exactly as long as the command does. That
//! replaces the usual shell dance — start a server in the background, poll for
//! a port file, export `REDIS_URL`, remember to kill it afterwards — with a
//! single foreground process that cleans up after itself.
//!
//! Because the port is resolved *before* the child is spawned, there is no
//! startup race to lose: by the time the command can run, the server is already
//! accepting connections.

use std::process::ExitStatus;
use tokio::process::{Child, Command};

/// What to run, and where to tell it to connect.
pub struct Spec {
    pub command: String,
    pub args: Vec<String>,
    /// Extra environment variables that should also receive the connection
    /// URL, for apps that read something other than `REDIS_URL`.
    pub extra_env: Vec<String>,
}

impl Spec {
    /// The command as a single line, for the banner.
    pub fn display(&self) -> String {
        std::iter::once(&self.command)
            .chain(self.args.iter())
            .map(|part| {
                if part.is_empty() || part.contains(char::is_whitespace) {
                    format!("{part:?}")
                } else {
                    part.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// The address a child should dial, given what the server bound to. A wildcard
/// bind is reachable from anywhere, but the child is local, so point it at the
/// loopback rather than at `0.0.0.0` — which is not a connectable address.
pub fn connect_host(bind: &str) -> &str {
    match bind {
        "0.0.0.0" => "127.0.0.1",
        "::" | "[::]" => "::1",
        other => other,
    }
}

/// Build the `redis://` URL clients expect. IPv6 literals are bracketed, and
/// the password is percent-encoded so a punctuation-heavy one cannot corrupt
/// the URL's structure.
fn url(host: &str, port: u16, password: Option<&str>) -> String {
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    match password {
        Some(pw) => format!("redis://:{}@{host}:{port}", encode(pw)),
        None => format!("redis://{host}:{port}"),
    }
}

/// Percent-encode everything outside RFC 3986's unreserved set. Conservative on
/// purpose: over-encoding is always safe to decode, under-encoding is not.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Start the child with the connection details in its environment. stdin,
/// stdout and stderr are inherited, so the command behaves exactly as it would
/// without the wrapper.
pub fn spawn(spec: &Spec, host: &str, port: u16, password: Option<&str>) -> std::io::Result<Child> {
    let url = url(host, port, password);
    let mut cmd = Command::new(&spec.command);
    cmd.args(&spec.args)
        .env("REDIS_URL", &url)
        .env("REDIS_HOST", host)
        .env("REDIS_PORT", port.to_string());
    for name in &spec.extra_env {
        cmd.env(name, &url);
    }
    cmd.spawn()
}

/// Wait for the child, forwarding stop signals to it, and report the exit code
/// the wrapper should exit with.
///
/// On a terminal Ctrl-C the child has already been signalled directly — it
/// shares our process group — so the forwarded signal is redundant but
/// harmless, and it is what makes `kill <meebis-pid>` work when meebis was
/// signalled alone. A second signal stops asking nicely.
pub async fn supervise(mut child: Child) -> i32 {
    let pid = child.id();
    let mut escalate = false;

    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => Some(s),
            Err(e) => {
                eprintln!("meebis: run: could not listen for SIGTERM: {e}");
                None
            }
        };
        loop {
            // `Child::wait` is cancel-safe: losing this branch to a signal
            // leaves the child unreaped, so the next pass can wait again.
            tokio::select! {
                status = child.wait() => return code(status),
                _ = tokio::signal::ctrl_c() => {
                    stop(pid, libc::SIGINT, &mut escalate, &mut child)
                }
                _ = async {
                    match sigterm.as_mut() {
                        Some(s) => { s.recv().await; }
                        None => std::future::pending::<()>().await,
                    }
                } => stop(pid, libc::SIGTERM, &mut escalate, &mut child),
            }
        }
    }

    #[cfg(not(unix))]
    {
        let _ = pid;
        loop {
            tokio::select! {
                status = child.wait() => return code(status),
                _ = tokio::signal::ctrl_c() => {
                    if escalate {
                        let _ = child.start_kill();
                    }
                    escalate = true;
                }
            }
        }
    }
}

/// Ask the child to stop, then insist. `kill` on a pid we have not reaped is
/// safe: the process is either alive or a zombie, so the number cannot have
/// been recycled onto some unrelated process.
#[cfg(unix)]
fn stop(pid: Option<u32>, sig: i32, escalate: &mut bool, child: &mut Child) {
    if *escalate {
        let _ = child.start_kill();
        return;
    }
    *escalate = true;
    if let Some(pid) = pid {
        unsafe { libc::kill(pid as i32, sig) };
    }
}

/// Map the child's fate onto our own exit code, using the shell's convention of
/// `128 + signal` for a command that was killed rather than returning.
fn code(status: std::io::Result<ExitStatus>) -> i32 {
    match status {
        Ok(status) => match status.code() {
            Some(code) => code,
            None => {
                #[cfg(unix)]
                {
                    use std::os::unix::process::ExitStatusExt;
                    status.signal().map(|s| 128 + s).unwrap_or(1)
                }
                #[cfg(not(unix))]
                {
                    1
                }
            }
        },
        Err(e) => {
            eprintln!("meebis: run: could not wait for the command: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_without_password() {
        assert_eq!(url("127.0.0.1", 6400, None), "redis://127.0.0.1:6400");
    }

    #[test]
    fn url_brackets_ipv6() {
        assert_eq!(url("::1", 6379, None), "redis://[::1]:6379");
        // Already bracketed input is not double-wrapped.
        assert_eq!(url("[::1]", 6379, None), "redis://[::1]:6379");
    }

    #[test]
    fn url_encodes_the_password() {
        assert_eq!(
            url("127.0.0.1", 6379, Some("p@ss:w/rd")),
            "redis://:p%40ss%3Aw%2Frd@127.0.0.1:6379"
        );
        // Unreserved characters survive untouched.
        assert_eq!(
            url("127.0.0.1", 6379, Some("aZ0-._~")),
            "redis://:aZ0-._~@127.0.0.1:6379"
        );
    }

    #[test]
    fn wildcard_binds_become_loopback() {
        assert_eq!(connect_host("0.0.0.0"), "127.0.0.1");
        assert_eq!(connect_host("::"), "::1");
        assert_eq!(connect_host("127.0.0.1"), "127.0.0.1");
        assert_eq!(connect_host("192.168.1.5"), "192.168.1.5");
    }

    #[test]
    fn display_quotes_only_what_needs_it() {
        let spec = Spec {
            command: "npm".into(),
            args: vec!["test".into(), "a b".into()],
            extra_env: vec![],
        };
        assert_eq!(spec.display(), "npm test \"a b\"");
    }
}
