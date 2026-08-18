# meebis

A fast, disposable, in-memory **Redis-compatible** server in Rust — for
ephemeral local work.

Spin one up per git worktree, point a couple of processes at it, then throw it
away. It boots clean every time, keeps everything in RAM, and forgets it all on
exit. There is no config file and nothing to clean up. To skip the server
lifecycle entirely, `meebis run -- npm test` lends one command an instance and
takes it away again — see [`meebis run`](#meebis-run--one-command-one-instance).
If you *want* the keyspace to survive a restart, `--dumpfile` reads and writes
Redis' own RDB snapshot format — see [Snapshots](#snapshots-dumpfile).

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

**npm** — nothing to install if you only want to run one:

```sh
npx meebis                      # fetch and run, no install step
npm install --save-dev meebis   # or pin it into a project
```

Handy as a `package.json` script, so a checkout comes with its own Redis:

```json
"scripts": {
  "redis": "meebis --port 6400"
}
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

**mise** or **asdf** — pin a version per project, alongside your other tools:

```sh
mise use github:mileszim/meebis@0.12.0        # mise, no plugin needed

asdf plugin add meebis https://github.com/mileszim/meebis.git
asdf install meebis 0.12.0                    # asdf
```

See [mise & asdf](#mise--asdf) for `.tool-versions`, `mise.toml`, and the
`@latest` caveat.

**Devcontainer** — add it to `devcontainer.json`, see [Devcontainer](#devcontainer):

```json
"features": { "ghcr.io/mileszim/meebis/meebis:1": {} }
```

**Docker** (linux/amd64 & linux/arm64) — see [Docker](#docker) for Compose:

```sh
docker run --rm -p 6379:6379 ghcr.io/mileszim/meebis
```

**Prebuilt binaries** for macOS and Linux (arm64 & x86_64) are attached to every
[release](https://github.com/mileszim/meebis/releases/latest) as `.tar.xz`
archives, with `sha256` checksums. The shell and npm installers both just fetch
the right one of these — there is no compile step and no build toolchain needed.

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
meebis run -- npm test          # or skip all that: see `meebis run` below
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
| `--unixsocket <PATH>` | *(none)* | Listen on a unix socket; on its own it replaces the port (see [Unix sockets](#unix-sockets)) |
| `--port-file <PATH>` | *(none)* | Write the actual listen port to `<PATH>` on boot |
| `--requirepass <PASS>` | *(none)* | Require `AUTH` before most commands |
| `--maxclients <N>` | `10000` | Maximum simultaneous connections |
| `--databases <N>` | `16` | Number of `SELECT`able databases |
| `--dumpfile <PATH>` | *(none)* | Load this RDB snapshot at boot, write it on exit (see [Snapshots](#snapshots-dumpfile)) |
| `--dir <DIR>` / `--dbfilename <NAME>` | `.` / `dump.rdb` | Redis' two-part spelling of `--dumpfile` |
| `--seed <PATH>` | *(none)* | Load this RDB snapshot at boot and never write to it (see [Seeding from a fixture](#seeding-from-a-fixture-seed)) |
| `--dumpfile-strict` | *(off)* | Refuse to start when the snapshot asked for cannot be loaded |
| `--verbose` | *(off)* | Log every command and reply (see [Verbose logging](#verbose-logging)) |
| `--loglevel <LEVEL>` | `notice` | `nothing`/`warning`/`notice`/`verbose`/`debug`; `verbose` and `debug` are the same as `--verbose` |
| `--env <NAME>` | *(none)* | `meebis run` only: also set `<NAME>` to the connection URL (repeatable) |
| `-h`, `--help` / `-v`, `--version` | | Print help / version |

Multiple processes can connect to the same instance concurrently and share the
keyspace, including pub/sub and transactions.

### `meebis run` — one command, one instance

Most of the time you don't want to manage a server at all; you want a command to
*have* a Redis. `meebis run` starts one, runs the command against it, and shuts
it down when the command exits:

```sh
meebis run -- npm test
```

The command is handed its connection details in the environment:

| Variable | Example |
|----------|---------|
| `REDIS_URL` | `redis://127.0.0.1:54312` |
| `REDIS_HOST` | `127.0.0.1` |
| `REDIS_PORT` | `54312` |

Without an explicit `--port` the OS picks a free one, and the port is resolved
*before* the command starts — so there is no startup race to lose, no port file
to poll, and no collision when several run at once:

```sh
meebis run -- pytest
meebis run -- ./bin/rails test
meebis run --requirepass hunter2 -- ./my-app
```

That makes it safe to run one per worktree, or several in the same CI job, with
nothing left behind either way. Server options go before the `--`; everything
after it belongs to the command.

`meebis run` exits with the command's own exit status, so it drops into a test
script or a CI step without changing what "failed" means. For an app that reads
something other than `REDIS_URL`, name the variable you want:

```sh
meebis run --env CACHE_URL --env SIDEKIQ_REDIS_URL -- ./bin/dev
```

To skip ports altogether, `--unixsocket` swaps the address for a path — see
[Unix sockets](#unix-sockets).

Ctrl-C does what you would expect: the command is signalled, gets its own chance
to shut down, and meebis follows it out — writing the snapshot on the way, if
you passed `--dumpfile`. A second Ctrl-C stops asking nicely. meebis' own output
goes to stderr in this mode, so the command keeps stdout to itself and
`meebis run -- ... > out.txt` captures only the command's output.

### A long-lived instance per worktree

When you want a server that outlives any single command — one per worktree that
several processes share — run it directly. Instead of hand-assigning a port to
each worktree, let the OS pick a free one with `--port 0` and record it with
`--port-file`, so your app and tests can find it:

```sh
meebis --port 0 --port-file .meebis-port
export REDIS_URL="redis://127.0.0.1:$(cat .meebis-port)"
```

The port is written to the file atomically once meebis has bound, and rewritten
on each boot — so a reader never sees a half-written value.

#### With direnv

A `.envrc` is a natural place to wire this in: the worktree gets a Redis on the
first `cd` into it, and every shell there sees the same one.

<!-- envrc-recipe -->

```sh
# .envrc — one meebis for this worktree, started on the first `cd` in.
#
# direnv re-runs this whenever it reloads, so it must be idempotent: start a
# server only when one is not already answering.
meebis_dir="$PWD/.meebis"
mkdir -p "$meebis_dir"

# Asking the server whether it is there beats checking a pid: a pid can be
# recycled onto something unrelated, and what matters is that a Redis answers.
meebis_answering() {
  [ -s "$meebis_dir/port" ] || return 1
  (exec 3<>"/dev/tcp/127.0.0.1/$(cat "$meebis_dir/port")") 2>/dev/null
}

if ! meebis_answering; then
  rm -f "$meebis_dir/port"
  nohup meebis --port 0 --port-file "$meebis_dir/port" >"$meebis_dir/log" 2>&1 &
  echo $! >"$meebis_dir/pid"
  # meebis writes the port file only once it has bound, so waiting for it is
  # waiting for "ready", not merely for "spawned".
  for _ in $(seq 1 50); do
    if meebis_answering; then break; fi
    sleep 0.1
  done
fi

if meebis_answering; then
  export REDIS_HOST=127.0.0.1
  export REDIS_PORT="$(cat "$meebis_dir/port")"
  export REDIS_URL="redis://$REDIS_HOST:$REDIS_PORT"
  # A restart picks a new port; this makes direnv notice instead of replaying a
  # stale one from its cache.
  if declare -F watch_file >/dev/null; then watch_file "$meebis_dir/port"; fi
else
  echo "meebis: did not come up — see $meebis_dir/log" >&2
fi
```

Then `direnv allow`, and add `.meebis/` to `.gitignore`.

Two things this deliberately does not pretend to do:

- **It does not stop the server when you leave.** direnv has no reliable
  teardown hook, so the instance outlives the shell — which is usually what you
  want for a worktree, at about 2 MB. `kill $(cat .meebis/pid)` when you're done
  with the worktree, or just delete the worktree and let it go.
- **direnv caches the environment**, so if the server dies while you are away,
  the variables it exported can outlive it. `direnv reload` starts a fresh one.

If neither of those appeals, a test command doesn't need any of this —
`meebis run -- <command>` starts and stops its own instance, which is why it is
the better default for anything that isn't a long-lived shell.

#### With a Procfile

If the worktree already runs its processes under `foreman`, `overmind` or
`hivemind`, meebis is just another line, and the supervisor handles the
lifecycle:

```procfile
redis: meebis --port 6400
web:   ./bin/dev
```

Pick a distinct port per worktree, or use `--unixsocket .meebis/redis.sock` and
skip the question entirely.

### Unix sockets

A port has to be allocated, recorded, and looked up. A socket path doesn't —
it's derivable from the worktree itself, which makes it the simpler address for
exactly the case meebis is built for:

```sh
meebis --unixsocket .meebis/redis.sock
```

```
meebis 0.9.0 ready on .meebis/redis.sock (pid 12345) — in-memory, no persistence
```

**`--unixsocket` on its own replaces the TCP port entirely.** That is the point:
leaving `6379` bound as well would reintroduce the collision the socket was
chosen to avoid, so twenty worktrees can each run that exact command with
nothing to coordinate. Pass `--port` as well to listen on both:

```sh
meebis --unixsocket .meebis/redis.sock --port 6400
```

Clients dial it the way they dial Redis:

```sh
redis-cli -s .meebis/redis.sock ping
```

```python
r = redis.Redis(unix_socket_path=".meebis/redis.sock")
r = redis.from_url("unix:///abs/path/to/redis.sock")   # or by URL
```

`meebis run` works the same way, and hands down what there is to hand down —
`REDIS_URL` as `unix://<path>` and `REDIS_SOCKET` as the bare path. `REDIS_HOST`
and `REDIS_PORT` are *unset* in this mode rather than left holding whatever they
held before, so an app that needs a host:port fails where the mistake is instead
of quietly connecting to a different Redis:

```sh
meebis run --unixsocket .meebis/redis.sock -- npm test
```

The socket file is removed on a clean exit. If meebis is killed outright it
stays behind, so the next boot clears it — but only after checking that nobody
is answering on it, which means a second server can never displace a running
one. A path holding something that isn't a socket is refused, never deleted.

Unix only, for the obvious reason; on Windows `--unixsocket` is rejected at
startup.

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

## Docker

Multi-arch images (`linux/amd64`, `linux/arm64`) are published to the GitHub
Container Registry on every release:

```sh
docker run --rm -p 6379:6379 ghcr.io/mileszim/meebis
```

| Tag | Points at |
|-----|-----------|
| `0.8.0` | that exact release |
| `0.8` | the newest patch of that minor |
| `latest` | the tip of `main` |
| `main`, `sha-<short>` | the tip of `main`, and each individual commit |

`latest` follows `main` rather than the newest release, so pin to `0.8` (or a
full version) for anything you want to stay put.

The image is built `FROM scratch` around a statically linked musl binary — no
shell, no package manager, nothing but meebis, at roughly the size of the binary
itself. The entrypoint already passes `--bind 0.0.0.0`, so the server is
reachable from outside the container; any flags you add are appended to it:

```sh
docker run --rm -p 6400:6379 ghcr.io/mileszim/meebis --requirepass hunter2 --verbose
```

### Compose

meebis is a drop-in swap for a `redis` service. The whole point is that there is
nothing else to configure — no volume, no `command:` override, no health-check
wait, because a fresh instance is the desired state every time:

```yaml
services:
  redis:
    image: ghcr.io/mileszim/meebis:0.8
    ports: ["6379:6379"]

  app:
    build: .
    environment:
      REDIS_URL: redis://redis:6379
    depends_on: [redis]
```

Two things to know, both consequences of the `scratch` image:

- **No in-container health check.** There is no shell and no `redis-cli` inside,
  so the usual `test: ["CMD", "redis-cli", "ping"]` will not run. meebis binds
  its port before it prints its banner and does no startup work unless you pass
  `--dumpfile`, so plain `depends_on` is normally enough; if you need a real
  gate, run the probe from the depending service instead.
- **`docker exec` gets you nothing.** Use `redis-cli` from the host (or another
  service) against the published port.

### Snapshots in a container

Nothing is written to disk by default. To carry a keyspace across container
restarts, mount a directory and point `--dumpfile` into it:

```sh
docker run --rm -p 6379:6379 -v "$PWD/.meebis:/data" \
  ghcr.io/mileszim/meebis --dumpfile /data/dump.rdb
```

`docker stop` sends `SIGTERM`, which meebis catches and snapshots on; `docker
kill` does not. See [Snapshots](#snapshots-dumpfile) for what that file is and
is not.

## mise & asdf

If your project already pins its toolchain, meebis can be one more line in it —
which is the natural fit, since the audience for a per-worktree Redis is largely
the audience already running a version manager.

### mise

No plugin required: mise can install straight from the release assets.

```sh
mise use github:mileszim/meebis@0.12.0
```

```toml
# mise.toml
[tools]
"github:mileszim/meebis" = "0.12.0"
```

`@latest` also works, with one wrinkle worth knowing: mise's
`minimum_release_age` setting hides releases newer than a few days, so
immediately after a release `@latest` still resolves to the previous one. Pin
the version, or clear the setting, if that matters to you.

meebis also hosts an asdf plugin (below) that mise can use instead, if you would
rather refer to it by bare name:

```sh
mise plugin add meebis https://github.com/mileszim/meebis.git
mise use meebis@0.12.0
```

### asdf

The plugin lives in this repository rather than a separate `asdf-meebis` one, so
there is nothing extra to trust:

```sh
asdf plugin add meebis https://github.com/mileszim/meebis.git
asdf install meebis 0.12.0
asdf set meebis 0.12.0          # writes .tool-versions
```

```
# .tool-versions
meebis 0.12.0
```

It installs the prebuilt binary for your platform (macOS and Linux, arm64 and
x86_64) and checks it against the `sha256` published beside it. `asdf install
meebis latest` picks the newest release.

Neither route builds from source, so neither needs a Rust toolchain.

## Devcontainer

There is a [devcontainer feature](https://containers.dev/features) that puts
meebis in the image:

```json
{
  "image": "mcr.microsoft.com/devcontainers/base:debian",
  "features": {
    "ghcr.io/mileszim/meebis/meebis:1": {}
  }
}
```

Pin the version if you want to:

```json
"features": {
  "ghcr.io/mileszim/meebis/meebis:1": { "version": "0.12.0" }
}
```

It installs the binary and **starts nothing** — meebis has no daemon, no data
directory, and no config, so there is nothing to keep running between uses. Wrap
whatever needs a Redis:

```json
"postCreateCommand": "meebis run -- npm test"
```

...or start one for the life of the container, if several processes need to
share it:

```json
"postStartCommand": "nohup meebis --port 6379 > /tmp/meebis.log 2>&1 &"
```

**The base image needs glibc 2.34 or newer** — Debian 12+, Ubuntu 22.04+ — since
that is what the release binaries are linked against. On an older base, or on
Alpine, the feature stops during the build and says so rather than installing
something that cannot run. For a musl or minimal environment, use the
[Docker image](#docker) instead: it is statically linked and has no such
requirement.

## Claude Code plugin

Coding agents working in parallel worktrees hit the same problem this whole tool
is about: each one needs its own Redis, and none of them should be sharing a
keyspace. There is a plugin in [`claude-code/`](claude-code/) for that:

```
/plugin marketplace add mileszim/meebis
/plugin install meebis@meebis
```

It ships a **skill** that tells Claude to reach for `meebis run -- <command>`,
where to find a running instance's port, and — the part that saves the most
time — which Redis features meebis does not implement, so a missing `XREADGROUP`
isn't mistaken for a bug in the code under test.

It also ships **session hooks** that give a project its own instance for as long
as a session lasts. Those are opt-in per project, so installing the plugin does
not start servers everywhere:

```sh
mkdir .meebis            # this project wants an instance
echo .meebis/ >> .gitignore
```

Every Claude Code session in that project then gets a server on an OS-assigned
port, recorded in `.meebis/port`; sessions in the same project share one, and it
is stopped when the last of them ends. See the
[plugin README](claude-code/README.md) for the details.

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

### Seeding from a fixture (`--seed`)

`--dumpfile` reads *and* writes the same path, which is wrong for the obvious
worktree pattern — "every instance starts from `fixtures/golden.rdb`" — because
every instance would then overwrite the fixture on the way out, and the third
worktree would inherit whatever the second one happened to leave behind.

`--seed` is the read-only half:

```sh
meebis --seed fixtures/golden.rdb
```

```
meebis 0.9.0 ready on 127.0.0.1:6379 (pid 12345) — in-memory, seeded from fixtures/golden.rdb (read-only)
```

The snapshot is loaded at boot and the file is never written to again — not on
exit, not on `SHUTDOWN`, not on `SAVE`/`BGSAVE` (which still reply `OK`, exactly
as they do for an instance with no dump file at all). Run twenty of them against
one fixture and it is still byte-for-byte the file you committed.

It also declines the one other thing `--dumpfile` does to its file: a dump that
won't load is renamed to `<name>.unreadable-<pid>` so the next save can't destroy
it, but a seed that won't load is *left alone* — it isn't meebis' file to move,
and twenty instances must not race to shuffle it around. meebis warns and starts
empty instead.

`--seed` and `--dumpfile` (and its `--dir`/`--dbfilename` spelling) are mutually
exclusive; they say opposite things about the same path. `--dumpfile-strict`
applies to either, and for a seed it also makes a *missing* file fatal — an
absent dump file is an ordinary first boot, while an absent fixture is a path
that doesn't say what its author thought it said.

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
