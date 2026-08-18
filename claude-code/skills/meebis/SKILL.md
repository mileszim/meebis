---
name: meebis
description: This skill should be used when local development or test work needs a Redis — running tests that talk to Redis, starting a scratch Redis, pointing an app at one in a git worktree, or tracing what an app actually sends to Redis. meebis is a disposable in-memory Redis-compatible server; this covers how to start one, how to find its port, and which Redis features it does not implement.
---

# meebis

meebis is a Redis-compatible server that boots clean, keeps everything in RAM,
and forgets it on exit. It exists so a worktree, a test run, or a session can
have its own Redis without installing, configuring, or cleaning up anything.

It is not a Redis replacement. It is a dev tool, and it is not durable.

## Which of the two shapes to use

### A command that needs a Redis — use `meebis run`

This is almost always the right answer. It starts a server, runs the command
against it, and takes the server away when the command exits:

```sh
meebis run -- npm test
meebis run -- pytest
meebis run -- ./bin/rails test
```

The command is handed its connection details in the environment — `REDIS_URL`,
`REDIS_HOST`, `REDIS_PORT` — with the port resolved *before* it starts, so there
is nothing to poll for and no startup race. `meebis run` exits with the
command's own exit status, so it does not change what "the tests failed" means.

Prefer this over starting a server in the background. It cannot leak a process,
cannot collide with another instance, and needs no cleanup step.

For an app that reads a differently-named variable:

```sh
meebis run --env CACHE_URL --env SIDEKIQ_REDIS_URL -- ./bin/dev
```

### A server several processes share — read `.meebis/port`

If this project has a `.meebis/` directory, a server is already running for the
session and its port is in `.meebis/port`. Read the file; do not hard-code the
value, and do not assume 6379:

```sh
redis-cli -p "$(cat .meebis/port)" ping
```

That instance is shared by every Claude Code session open on the project and is
stopped when the last one ends. Its keyspace is scratch space — do not put
anything in it that needs to survive.

To start a long-lived one by hand elsewhere:

```sh
meebis --port 0 --port-file .meebis-port    # OS picks a free port, records it
```

## Tracing what an app does to Redis

When the question is "what is this app actually sending?", `--verbose` logs
every command and reply, with the client id and how long it took — including the
commands a Lua script issues:

```sh
meebis --port 6400 --verbose
```

It can also be turned on and off on a running server, which is the better move
against a shared instance:

```sh
redis-cli -p "$(cat .meebis/port)" config set loglevel verbose
redis-cli -p "$(cat .meebis/port)" config set loglevel notice
```

This is usually faster than adding logging to the app.

## What meebis does not implement

Reaching for one of these will fail, and the failure will look like a bug in the
code under test. Check here first:

- **Stream consumer groups** — `XGROUP`, `XREADGROUP`, `XACK`, `XPENDING`,
  `XCLAIM`, `XAUTOCLAIM`. Plain `XADD`/`XREAD`/`XRANGE`/`XTRIM`/`XDEL` work.
- **The blocking list commands** — `BLPOP`, `BRPOP`, `BLMOVE`, `BLMPOP`,
  `BZMPOP`. `BZPOPMIN`/`BZPOPMAX` and `XREAD BLOCK` do work.
- **HyperLogLog** (`PFADD` and friends), **GEO** commands, and **cluster** mode.
- **Durability** — there are no save points and no AOF. `--dumpfile` reads and
  writes Redis' RDB format on boot and clean exit, which carries state across a
  restart but is not crash safety.

Everything else in common use is supported and verified byte-for-byte against
Redis 7.2: strings, bitmaps, keys and expiry, hashes, lists, sets, sorted sets,
streams, `EVAL`/`EVALSHA` with real Lua 5.1, pub/sub, transactions, and the 16
numbered databases. RESP2 and RESP3 both work, so standard clients need no
special configuration.

If a command's behavior is in question, the authority is real Redis: meebis'
test suite diffs it against `redis-server` command for command, and any
divergence is a bug worth reporting rather than working around.

## Checking the installed version

Flags differ between versions; `meebis --help` is the answer for the version
actually installed:

```sh
meebis --version
meebis --help
```
