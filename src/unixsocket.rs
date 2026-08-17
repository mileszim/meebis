//! Listening on a unix-domain socket.
//!
//! For the one-instance-per-worktree case a socket path is a better address
//! than a port. The path is derivable from the worktree itself, so there is
//! nothing to allocate, nothing to discover, and nothing to collide over:
//! twenty worktrees each running `meebis --unixsocket .meebis/redis.sock` is
//! twenty servers that never had to agree on anything.
//!
//! The cost is a file that outlives the process when it dies badly, which is
//! what [`bind`] exists to sort out.

use std::path::Path;

/// Bind a listener, clearing a stale socket file left behind by a previous run.
///
/// A leftover file is the ordinary aftermath of `kill -9` — nothing gets the
/// chance to unlink it — and it makes `bind` fail with `EADDRINUSE`. Left
/// unhandled that means a worktree stays broken until someone deletes a file
/// they have no particular reason to know about.
///
/// Connecting first is what separates "stale" from "in use": only a socket
/// nobody answers on is removed, so a second server can never displace a
/// running one, and a path that happens to hold a real file is refused rather
/// than deleted.
pub fn bind(path: &Path) -> std::io::Result<tokio::net::UnixListener> {
    use std::os::unix::fs::FileTypeExt;

    match std::fs::metadata(path) {
        // Nothing there: the normal first boot.
        Err(_) => {}
        Ok(meta) if meta.file_type().is_socket() => {
            if std::os::unix::net::UnixStream::connect(path).is_ok() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    "a server is already listening on it",
                ));
            }
            std::fs::remove_file(path)?;
        }
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "the path already exists and is not a socket",
            ))
        }
    }
    tokio::net::UnixListener::bind(path)
}

/// Remove the socket on the way out so the path is free for the next run.
///
/// Best-effort on purpose: this runs on the exit path, where a failure to tidy
/// up is not worth refusing to exit over — and [`bind`] already copes with the
/// file being left behind.
pub fn cleanup(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// How a unix client is reported by `CLIENT LIST` / `CLIENT INFO`: the socket
/// path with Redis' placeholder `:0` port, since the peer has no address of
/// its own.
pub fn peer_addr(path: &Path) -> String {
    format!("{}:0", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temp directory that cleans itself up, so these tests leave nothing in
    /// the build tree.
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(tag: &str) -> TempDir {
            let dir = std::env::temp_dir().join(format!(
                "meebis-unixsocket-{}-{tag}-{}",
                std::process::id(),
                crate::db::now_ms()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }
        fn join(&self, name: &str) -> std::path::PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // The two tests that reach `UnixListener::bind` need a reactor to register
    // the socket with; the ones that are refused before that point do not.

    #[tokio::test]
    async fn binds_a_fresh_path() {
        let dir = TempDir::new("fresh");
        let path = dir.join("redis.sock");
        let listener = bind(&path).expect("a fresh path should bind");
        assert!(path.exists(), "the socket should exist once bound");
        drop(listener);

        cleanup(&path);
        assert!(!path.exists(), "cleanup should remove the socket");
    }

    #[tokio::test]
    async fn a_stale_socket_is_replaced() {
        let dir = TempDir::new("stale");
        let path = dir.join("redis.sock");

        // Bind and drop without unlinking — exactly what a `kill -9` leaves.
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        drop(listener);
        assert!(path.exists(), "the stale file should still be there");

        bind(&path).expect("a stale socket should be cleared, not fatal");
    }

    #[test]
    fn a_live_socket_is_refused() {
        let dir = TempDir::new("live");
        let path = dir.join("redis.sock");

        // Held open for the duration, so a connect attempt succeeds and the
        // path is correctly read as in use.
        let _held = std::os::unix::net::UnixListener::bind(&path).unwrap();

        let err = bind(&path).expect_err("a live socket must not be stolen");
        assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);
        assert!(path.exists(), "the live socket must survive the attempt");
    }

    #[test]
    fn a_regular_file_is_never_deleted() {
        let dir = TempDir::new("file");
        let path = dir.join("not-a-socket");
        std::fs::write(&path, b"precious").unwrap();

        let err = bind(&path).expect_err("a regular file is not ours to remove");
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&path).unwrap(), b"precious");
    }

    #[test]
    fn peer_addr_matches_redis_spelling() {
        assert_eq!(peer_addr(Path::new("/tmp/redis.sock")), "/tmp/redis.sock:0");
    }
}
