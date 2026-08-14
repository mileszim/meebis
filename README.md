# meebis

A fast, disposable, in-memory **Redis-compatible** server in Rust — for
ephemeral local work.

Spin one up per git worktree, point a couple of processes at it, then throw it
away. It boots clean every time, keeps everything in RAM, and forgets it all on
exit. There is no config file and nothing to clean up. If you *want* the
keyspace to survive a restart, `--dumpfile` reads and writes Redis' own RDB
snapshot format — see [Snapshots](#snapshots-dumpfile).

- **Fast** — matches real Redis throughput (~110–130k ops/sec single-threaded,
  sub-millisecond latency).
- **Tiny** — a sub-1 MB binary using ~2 MB RAM per instance idle, so you can run
  dozens at once without noticing (see [Footprint & performance](#footprint--performance)).
- **Compatible** — speaks RESP2 and RESP3 and a broad slice of the Redis
  command surface. `redis-cli`, `redis-py`, and other standard client libraries
  just work, verified byte-for-byte against Redis 7.2.
- **Disposable** — clean on boot, gone on exit, unless you hand it a snapshot
  to load and save. Not durable either way, by design.

It is *not* a Redis replacement for production. It's a dev tool.

## Install

**Homebrew** (macOS & Linux):

```sh
brew install mileszim/tap/meebis
```

**Shell installer** (macOS & Linux) — downloads the right prebuilt binary for your platform:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/mileszim/meebis/releases/latest/download/meebis-installer.sh | sh
```

**Cargo**:

```sh
cargo binstall meebis           # prebuilt binary, no compile (needs cargo-binstall)
cargo install meebis            # build from source (crates.io)
```

**Prebuilt binaries** for macOS and Linux (arm64 & x86_64) are attached to every
[release](https://github.com/mileszim/meebis/releases/latest) as `.tar.xz`
archives, with `sha256` checksums.

**From a local checkout**:

```sh
cargo build --release           # ./target/release/meebis
cargo install --path .          # installs `meebis` into your cargo bin
```

## Run

```sh
meebis                          # listen on 127.0.0.1:6379
meebis --port 6400              # pick a port (the main thing you'll configure)
meebis --port 0                 # let the OS choose a free port (printed on boot)
meebis --requirepass hunter2    # require AUTH
```

```
meebis 0.1.0 ready on 127.0.0.1:6400 (pid 12345) — in-memory, no persistence
```

Then connect as you would to Redis:

```sh
redis-cli -p 6400 set hello world
redis-cli -p 6400 get hello
```

```python
import redis
r = redis.Redis(port=6400)      # redis-py, node-redis, go-redis, lettuce, ...
r.set("hello", "world")
```

### Options

| Flag | Default | Description |
|------|---------|-------------|
| `-p`, `--port <PORT>` | `6379` | Port to listen on (`0` lets the OS pick a free one) |
| `--bind <ADDR>` | `127.0.0.1` | Address to bind |
| `--port-file <PATH>` | *(none)* | Write the actual listen port to `<PATH>` on boot |
| `--requirepass <PASS>` | *(none)* | Require `AUTH` before most commands |
| `--maxclients <N>` | `10000` | Maximum simultaneous connections |
| `--databases <N>` | `16` | Number of `SELECT`able databases |
| `--dumpfile <PATH>` | *(none)* | Load this RDB snapshot at boot, write it on exit (see [Snapshots](#snapshots-dumpfile)) |
| `--dir <DIR>` / `--dbfilename <NAME>` | `.` / `dump.rdb` | Redis' two-part spelling of `--dumpfile` |
| `--dumpfile-strict` | *(off)* | Refuse to start when a dump exists but cannot be loaded |
| `--verbose` | *(off)* | Log every command and reply (see [Verbose logging](#verbose-logging)) |
| `--loglevel <LEVEL>` | `notice` | `nothing`/`warning`/`notice`/`verbose`/`debug`; `verbose` and `debug` are the same as `--verbose` |
| `-h`, `--help` / `-v`, `--version` | | Print help / version |

Multiple processes can connect to the same instance concurrently and share the
keyspace, including pub/sub and transactions.

### Discovering the port (one instance per worktree)

meebis is meant to be run one-per-worktree and thrown away. Instead of
hand-assigning a port to each worktree, let the OS pick a free one with
`--port 0` and record it with `--port-file`, so your app and tests can find it:

```sh
meebis --port 0 --port-file .meebis-port
export REDIS_URL="redis://127.0.0.1:$(cat .meebis-port)"
```

The port is written to the file atomically once meebis has bound, and rewritten
on each boot — so a reader never sees a half-written value. A `.envrc` (direnv)
or a `Procfile` is a natural place to wire this into your dev loop.

### Verbose logging

To see what your app is actually doing to Redis, start with `--verbose`:

```sh
meebis --port 6400 --verbose
```

Every command in and reply out is logged to stdout, tagged with the client id
that sent it and how long it took:

```
meebis 0.5.0 ready on 127.0.0.1:6400 (pid 12345) — in-memory, no persistence
2026-08-13T18:04:21.512Z * verbose logging on — every command and reply is logged
2026-08-13T18:04:21.583Z #1 * connected from 127.0.0.1:52814
2026-08-13T18:04:21.583Z #1 > SET greeting "hello world" EX 30
2026-08-13T18:04:21.583Z #1 < OK (21µs)
2026-08-13T18:04:21.584Z #1 > LRANGE queue 0 -1
2026-08-13T18:04:21.584Z #1 < [a, b, c] (6µs)
2026-08-13T18:04:21.585Z #1 > INCR greeting
2026-08-13T18:04:21.585Z #1 < (error) ERR value is not an integer or out of range (4µs)
2026-08-13T18:04:21.612Z #2 * connected from 127.0.0.1:52815
2026-08-13T18:04:21.612Z #2 > SUBSCRIBE news
2026-08-13T18:04:21.612Z #2 < (push) [subscribe, news, (integer) 1] (5µs)
2026-08-13T18:04:21.640Z #2 < (push) [message, news, hello]
2026-08-13T18:04:21.641Z #1 * disconnected
```

`>` is a command coming in, `<` a reply going out, `*` a connection event
(connected, disconnected, parked on a blocking command). Timestamps are UTC.
Values are quoted the way `redis-cli` shows them, long values and big replies
are truncated with a note of what was dropped, and passwords (`AUTH`,
`HELLO ... AUTH`, `CONFIG SET requirepass`) are logged as `<redacted>`.

Commands a Lua script issues are traced too, tagged `(lua)`, so an `EVAL` shows
its work instead of just its final reply:

```
2026-08-13T18:04:22.100Z #3 > EVAL "redis.call('set', KEYS[1], ARGV[1]) return redis.call('get', KEYS[1])" 1 k v
2026-08-13T18:04:22.100Z #3 > (lua) SET k v
2026-08-13T18:04:22.100Z #3 < (lua) OK (49µs)
2026-08-13T18:04:22.100Z #3 > (lua) GET k
2026-08-13T18:04:22.100Z #3 < (lua) v (2µs)
2026-08-13T18:04:22.101Z #3 < v (312µs)
```

It can also be flipped on and off on a running server, so you can leave a
long-lived instance quiet and only trace the moment you care about:

```sh
redis-cli -p 6400 config set loglevel verbose   # start logging
redis-cli -p 6400 config set loglevel notice    # stop
```

Logging is off by default and gated on a single atomic load, so a quiet server
runs at full speed; with it on, expect to give up some throughput to the
formatting and writing (~15% under `redis-benchmark`).

## Supported commands

Verified byte-for-byte against Redis 7.2 for the cases in the test suite.

- **Strings** — `GET` `SET` (`EX`/`PX`/`EXAT`/`PXAT`/`NX`/`XX`/`GET`/`KEEPTTL`)
  `SETNX` `SETEX` `PSETEX` `GETSET` `GETDEL` `GETEX` `APPEND` `STRLEN` `INCR`
  `DECR` `INCRBY` `DECRBY` `INCRBYFLOAT` `MGET` `MSET` `MSETNX` `GETRANGE`
  `SETRANGE` `SUBSTR`
- **Bitmaps** — `SETBIT` `GETBIT` `BITCOUNT` `BITPOS` `BITOP`
- **Keys** — `DEL` `UNLINK` `EXISTS` `EXPIRE` `PEXPIRE` `EXPIREAT` `PEXPIREAT`
  `TTL` `PTTL` `EXPIRETIME` `PEXPIRETIME` `PERSIST` `KEYS` `SCAN` `TYPE`
  `RENAME` `RENAMENX` `RANDOMKEY` `TOUCH` `COPY` `MOVE`
- **Hashes** — `HSET` `HMSET` `HSETNX` `HGET` `HMGET` `HDEL` `HGETALL` `HKEYS`
  `HVALS` `HLEN` `HEXISTS` `HSTRLEN` `HINCRBY` `HINCRBYFLOAT` `HSCAN` `HRANDFIELD`
- **Lists** — `LPUSH` `RPUSH` `LPUSHX` `RPUSHX` `LPOP` `RPOP` `LLEN` `LRANGE`
  `LINDEX` `LSET` `LREM` `LTRIM` `LINSERT` `LPOS` `RPOPLPUSH` `LMOVE`
- **Sets** — `SADD` `SREM` `SMEMBERS` `SISMEMBER` `SMISMEMBER` `SCARD` `SPOP`
  `SRANDMEMBER` `SMOVE` `SUNION` `SINTER` `SDIFF` `SUNIONSTORE` `SINTERSTORE`
  `SDIFFSTORE` `SINTERCARD` `SSCAN`
- **Sorted sets** — `ZADD` (`NX`/`XX`/`GT`/`LT`/`CH`/`INCR`) `ZREM` `ZSCORE`
  `ZMSCORE` `ZCARD` `ZCOUNT` `ZINCRBY` `ZRANK` `ZREVRANK` `ZRANGE` `ZREVRANGE`
  `ZRANGEBYSCORE` `ZREVRANGEBYSCORE` `ZRANGEBYLEX` `ZREVRANGEBYLEX` `ZLEXCOUNT`
  `ZPOPMIN` `ZPOPMAX` `BZPOPMIN` `BZPOPMAX` `ZREMRANGEBYRANK` `ZREMRANGEBYSCORE`
  `ZSCAN` `ZRANDMEMBER`
- **Streams** — `XADD` (`*`/`<ms>-*`/explicit IDs, `NOMKSTREAM`, `MAXLEN`/`MINID`
  with `~`/`=` and `LIMIT`) `XLEN` `XRANGE` `XREVRANGE` `XREAD` (`COUNT`, `BLOCK`,
  `$` snapshot) `XDEL` `XTRIM`
- **Scripting** — `EVAL` `EVALSHA` `EVAL_RO` `EVALSHA_RO` `SCRIPT LOAD/EXISTS/FLUSH`,
  with sandboxed Lua 5.1 (Redis' scripting version), `redis.call`/`pcall`/
  `error_reply`/`status_reply`/`sha1hex`/`log`, and the `cjson`/`cmsgpack`/`bit`
  libraries. Scripts run atomically under a single held keyspace lock — the same
  guarantee Redis' single thread provides.
- **Pub/Sub** — `SUBSCRIBE` `UNSUBSCRIBE` `PSUBSCRIBE` `PUNSUBSCRIBE` `PUBLISH`
  `PUBSUB`
- **Transactions** — `MULTI` `EXEC` `DISCARD` `WATCH` `UNWATCH`
- **Connection** — `PING` `ECHO` `HELLO` `AUTH` `SELECT` `QUIT` `RESET` `CLIENT`
- **Server** — `INFO` `CONFIG GET/SET` `DBSIZE` `FLUSHDB` `FLUSHALL` `SWAPDB`
  `TIME` `COMMAND` `DEBUG` `OBJECT` `MEMORY` `DBSIZE` `SHUTDOWN` `LOLWUT` (and
  `SAVE`, `BGSAVE`, etc. as accepted no-ops)

Keys and values are binary-safe. `EXPIRE` and friends work with the full
`NX`/`XX`/`GT`/`LT` option set. Expired keys are removed lazily on access and by
a once-per-second sweep.

### Numbered databases

meebis provides Redis' 16 `SELECT`able databases (`--databases N` to change the
count). They are fully independent — `KEYS`, `DBSIZE`, `RANDOMKEY`, `FLUSHDB`
and `WATCH` are all scoped to the selected one, while `FLUSHALL` clears every
database. Keys cross between them with `MOVE key db` and `COPY src dst DB n`,
both of which carry the TTL over, and `SWAPDB` exchanges two wholesale.

Pub/Sub is global, as in Redis: a message published on one database is delivered
to subscribers on all of them.

## Snapshots (`--dumpfile`)

By default nothing touches the disk. Point meebis at a dump file and it will
load that snapshot at boot and write it back on the way out:

```sh
meebis --port 6400 --dumpfile .meebis/dump.rdb
```

```
meebis 0.7.0 ready on 127.0.0.1:6400 (pid 12345) — in-memory, snapshotting to .meebis/dump.rdb
```

The file is Redis' own **RDB format**, not a meebis invention, so it moves in
both directions:

```sh
# Seed a worktree from a snapshot a real Redis wrote
redis-cli -p 6379 save
meebis --port 6400 --dumpfile /var/lib/redis/dump.rdb

# ...or hand meebis' state to a real redis-server
redis-cli -p 6400 shutdown
redis-server --dir .meebis --dbfilename dump.rdb
```

Redis' own `--dir` / `--dbfilename` spelling works too, for tooling that already
sets them, and `CONFIG GET dir dbfilename` reports the real values.

The snapshot is written on a clean exit (`SIGINT`, `SIGTERM`, `SHUTDOWN`) and on
demand via `SAVE` / `BGSAVE`. meebis is single-threaded, so `BGSAVE` writes
synchronously and then returns Redis' reply — the file is already on disk when
the client hears back.

**This is not durability.** The keyspace still lives only in RAM, there are no
periodic save points, and a `kill -9` loses everything since the last save. What
it buys is carrying state *across* a restart or *between* servers.

A few things worth knowing:

- **Reading accepts anything; writing stays simple.** meebis reads every
  encoding across RDB versions 1–11 — listpacks, quicklists, ziplists, intsets,
  LZF-compressed strings — because Redis picks those and meebis has to cope. It
  *writes* only the flat, oldest-spelling encodings, which Redis 7.2+ still
  loads. Dumps are therefore a little larger than Redis' own.
- **A dump that won't load doesn't block boot.** meebis logs the reason, renames
  the unreadable file to `<name>.unreadable-<pid>` so the next save can't destroy
  it, and starts with an empty keyspace. `--dumpfile-strict` makes it exit
  instead, the way Redis does.
- **Consumer groups and functions are dropped**, with a warning, since meebis
  models neither. Everything else — every value type, TTLs, and all
  `--databases` — round-trips.
- **`DEBUG RELOAD`** runs the whole keyspace through the same codec in memory,
  which is how the test suite checks the two halves agree.

## Deliberately not supported

This is a small dev tool, so some Redis features are intentionally absent:

- **Durable persistence** — `--dumpfile` reads and writes RDB snapshots, but
  there are no save points, no AOF, and no crash safety.
- **Stream consumer groups** (`XGROUP`, `XREADGROUP`, `XACK`, `XPENDING`,
  `XCLAIM`, `XAUTOCLAIM`) — `XADD`/`XREAD`/`XRANGE`/`XTRIM`/`XDEL` are
  supported; groups are not.
- **List-blocking commands** (`BLPOP`, `BRPOP`, `BLMOVE`, `BLMPOP`, `BZMPOP`) —
  `BZPOPMIN`/`BZPOPMAX` and `XREAD BLOCK` are supported; the rest are not yet.
- **HyperLogLog**, **GEO**, and **cluster** mode.

Both RESP2 and RESP3 are supported — clients using either (e.g. `redis-py`'s
default RESP3, or `redis-cli`'s RESP2) work without configuration.

`WATCH` is implemented by fingerprinting watched keys and aborting `EXEC` if any
changed — correct for optimistic-locking patterns, without per-key versioning.

## Footprint & performance

meebis is built to be cheap enough to run many instances at once. Measured with
a `--release` build on an Apple Silicon laptop (12 cores):

| Metric | meebis | Notes |
|--------|--------|-------|
| Binary size | ~860 KB | one small binary, stripped |
| Idle memory (RSS) | ~2 MB per instance | one OS thread per process |
| 20 instances at once | ~40 MB total | dozens is a non-issue |
| Throughput | ~110–130k ops/sec | `redis-benchmark -n 100000 -c 50` |
| Latency | ~0.2 ms p50 | |

Command throughput and latency track real Redis 7.2 on the same machine — both
execute commands on a single thread — so meebis is not a local-dev bottleneck.
A side-by-side `redis-benchmark` run:

```
              meebis        redis 7.2
SET       121,212 rps      118,906 rps
GET       114,025 rps      123,001 rps
INCR      111,857 rps      125,471 rps
RPUSH     126,422 rps      123,305 rps
SADD      128,041 rps      123,001 rps
HSET      116,959 rps      104,822 rps
ZADD      121,951 rps      113,250 rps
```

Absolute numbers vary with hardware; the point is that the memory footprint
stays flat whether you run one instance or twenty, and speed is on par with
Redis itself.

## How it works

One `tokio` current-thread runtime per process (a single OS thread — hence the
tiny footprint), with all command execution serialized behind one mutex, just
like Redis. Each connection is an async task; pub/sub messages are pushed to
subscribers over in-process channels.

Lua scripts (`EVAL`) run under an embedded Lua 5.1 (Redis' scripting version),
holding that same keyspace mutex for the whole script — so each `redis.call`
inside a script re-enters the dispatcher against the locked keyspace with no
opportunity for another connection to interleave. Blocking commands
(`BZPOPMIN`, `XREAD BLOCK`) release the mutex and park on a shared `Notify`
that every write wakes, so idle waiters cost zero CPU.

## Development

```sh
cargo test          # unit tests for the protocol, glob matching, expiry, zsets, sha1
cargo clippy        # clean
```

The full Redis compatibility suite lives in `tests/compat/` — RESP2 fixtures
diffed byte-for-byte against a real `redis-server`, plus a RESP3 parity check
via `redis-py`. Run it with `bash tests/compat/run.sh` (needs `redis-server`
and `redis-cli` on PATH; the RESP3 stage needs `python3` with the `redis`
package).

## License

MIT — see [LICENSE](LICENSE).
