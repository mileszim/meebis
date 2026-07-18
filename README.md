# meebis

A fast, disposable, in-memory **Redis-compatible** server in Rust — for
ephemeral local work.

Spin one up per git worktree, point a couple of processes at it, then throw it
away. It boots clean every time, keeps everything in RAM, and forgets it all on
exit. There is no persistence, no config file, and nothing to clean up.

- **Fast** — matches real Redis throughput (~110–130k ops/sec single-threaded,
  sub-millisecond latency).
- **Tiny** — a sub-1 MB binary using ~2 MB RAM per instance idle, so you can run
  dozens at once without noticing (see [Footprint & performance](#footprint--performance)).
- **Compatible** — speaks RESP2 and RESP3 and a broad slice of the Redis
  command surface. `redis-cli`, `redis-py`, and other standard client libraries
  just work, verified byte-for-byte against Redis 7.2.
- **Disposable** — clean on boot, gone on exit. No durability, by design.

It is *not* a Redis replacement for production. It's a dev tool.

## Build & install

```sh
cargo build --release           # ./target/release/meebis
cargo install --path .          # installs `meebis` into ~/.cargo/bin
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
| `-p`, `--port <PORT>` | `6379` | Port to listen on |
| `--bind <ADDR>` | `127.0.0.1` | Address to bind |
| `--requirepass <PASS>` | *(none)* | Require `AUTH` before most commands |
| `--maxclients <N>` | `10000` | Maximum simultaneous connections |
| `-h`, `--help` / `-v`, `--version` | | Print help / version |

Multiple processes can connect to the same instance concurrently and share the
keyspace, including pub/sub and transactions.

## Supported commands

Verified byte-for-byte against Redis 7.2 for the cases in the test suite.

- **Strings** — `GET` `SET` (`EX`/`PX`/`EXAT`/`PXAT`/`NX`/`XX`/`GET`/`KEEPTTL`)
  `SETNX` `SETEX` `PSETEX` `GETSET` `GETDEL` `GETEX` `APPEND` `STRLEN` `INCR`
  `DECR` `INCRBY` `DECRBY` `INCRBYFLOAT` `MGET` `MSET` `MSETNX` `GETRANGE`
  `SETRANGE` `SUBSTR`
- **Bitmaps** — `SETBIT` `GETBIT` `BITCOUNT` `BITPOS` `BITOP`
- **Keys** — `DEL` `UNLINK` `EXISTS` `EXPIRE` `PEXPIRE` `EXPIREAT` `PEXPIREAT`
  `TTL` `PTTL` `EXPIRETIME` `PEXPIRETIME` `PERSIST` `KEYS` `SCAN` `TYPE`
  `RENAME` `RENAMENX` `RANDOMKEY` `TOUCH` `COPY`
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
  `ZPOPMIN` `ZPOPMAX` `ZREMRANGEBYRANK` `ZREMRANGEBYSCORE` `ZSCAN`
  `ZRANDMEMBER`
- **Pub/Sub** — `SUBSCRIBE` `UNSUBSCRIBE` `PSUBSCRIBE` `PUNSUBSCRIBE` `PUBLISH`
  `PUBSUB`
- **Transactions** — `MULTI` `EXEC` `DISCARD` `WATCH` `UNWATCH`
- **Connection** — `PING` `ECHO` `HELLO` `AUTH` `SELECT` `QUIT` `RESET` `CLIENT`
- **Server** — `INFO` `CONFIG GET/SET` `DBSIZE` `FLUSHDB` `FLUSHALL` `TIME`
  `COMMAND` `DEBUG` `OBJECT` `MEMORY` `DBSIZE` `SHUTDOWN` `LOLWUT` (and `SAVE`,
  `BGSAVE`, etc. as accepted no-ops)

Keys and values are binary-safe. `EXPIRE` and friends work with the full
`NX`/`XX`/`GT`/`LT` option set. Expired keys are removed lazily on access and by
a once-per-second sweep.

## Deliberately not supported

This is a small dev tool, so some Redis features are intentionally absent:

- **Persistence** (RDB/AOF) — everything is in memory and lost on exit.
- **Lua scripting** (`EVAL`), **Streams** (`XADD`...), **blocking commands**
  (`BLPOP`...), **HyperLogLog**, **GEO**, and **cluster** mode.
- **Numbered databases** — `SELECT` is accepted but there is a single shared
  keyspace. `FLUSHDB` and `FLUSHALL` both clear it.

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

## Development

```sh
cargo test          # unit tests for the protocol, glob matching, expiry, zsets
cargo clippy        # clean
```

## License

MIT — see [LICENSE](LICENSE).
