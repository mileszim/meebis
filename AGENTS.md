# Agent guide for meebis

meebis is a fast, disposable, in-memory **Redis-compatible** server in Rust.
See [README.md](README.md) for the user-facing overview. This file is for
coding agents (and humans) working *on* the project.

## Golden rule: stay compatible with Redis

meebis exists to behave like Redis. The test suite proves it by diffing meebis
against a real `redis-server`, command for command. **Any change to command
behavior must keep that parity.** When you add or change a command:

1. Check the real Redis behavior (`redis-cli` against a real server, or the
   Redis docs) — including error messages, which are asserted exactly.
2. Add cases to the fixtures in `tests/compat/resp2/*.txt` (deterministic
   output only — no timing-dependent values like exact `PTTL`) and, for
   RESP3-shaped replies, to `tests/compat/resp3_parity.py`.
3. Run the suite (below) and make sure it passes.

## Project layout

```
src/
  main.rs             entrypoint: arg parsing, runtime, connection loop
  resp.rs             RESP2/RESP3 protocol: Frame type + parser
  db.rs               keyspace, Value types, expiry, sorted set, glob match
  server.rs           shared state + per-connection state
  pubsub.rs           pub/sub registry
  runner.rs           `meebis run`: the supervised child and its environment
  unixsocket.rs       `--unixsocket`: binding, stale sockets, cleanup
  commands/
    mod.rs            dispatch, auth gate, transactions, pub/sub commands
    string.rs bitops.rs generic.rs hash.rs list.rs set.rs zset.rs
    clientcmd.rs       connection commands (PING/HELLO/AUTH/CLIENT/...)
    admin.rs           server commands (INFO/CONFIG/COMMAND/OBJECT/SAVE/...)
  rdb/
    mod.rs            load/save entry points, error and stats types
    cursor.rs         bounds-checked reader + the length/string codecs
    crc64.rs lzf.rs   checksum and decompression primitives
    packed.rs         listpack/ziplist/intset decoders, listpack writer
    read.rs write.rs  value codecs (read accepts all; write stays flat)
    stream.rs         the one type with no plain encoding
bin/                  the asdf plugin (asdf clones this repo and looks here)
tests/compat/         Redis-spec differential + RESP3 parity + RDB interchange
```

`bin/` is not project tooling — it is the asdf plugin contract (`list-all`,
`download`, `install`, `latest-stable`), which is why it sits at the repository
root: `asdf plugin add meebis <this repo>` expects to find it there. Exercise it
with `bash tests/asdf-plugin.sh`.

### The RDB codec is asymmetric on purpose

`rdb::read` accepts every encoding across RDB versions 1–11, because Redis
chooses the encoding and a real dump *will* contain listpacks, quicklists, and
intsets. `rdb::write` emits only the flat, oldest-spelling forms (types 0/1/2/4/5),
which current Redis still loads — verified against 7.2. Do not "modernize" the
writer to emit packed encodings; that trades a large amount of new code for
smaller files nobody is measuring.

Streams are the exception: `RDB_TYPE_STREAM_LISTPACKS*` has no flat form, so
`rdb::stream` is the only place meebis builds a listpack.

## Dev commands

```sh
cargo build --release
cargo test                              # unit tests (protocol, glob, expiry, zset)
cargo fmt --all                         # format (CI enforces --check)
cargo clippy --all-targets              # lint (CI enforces -D warnings)
bash tests/compat/run.sh ./target/release/meebis   # Redis-spec compatibility
bash tests/compat/rdb.sh ./target/release/meebis  # just the RDB interchange stage
```

The compatibility script needs `redis-server`/`redis-cli` on PATH; the RESP3
stage additionally needs `python3` with the `redis` package (it is skipped if
absent). CI installs all of these.

`run.sh` ends by calling `rdb.sh`, which starts its own servers to prove a
snapshot written by either server loads into the other. When you touch the
codec, run that stage directly — it is the only test that exercises the
encodings Redis emits but meebis never writes.

## Commit messages drive releases

This repo uses [Conventional Commits](https://www.conventionalcommits.org) with
[release-please](https://github.com/googleapis/release-please). On every push to
`main`, release-please maintains a **release PR** that collects changes and the
next version number. **Merging that release PR** is what tags the version,
publishes the GitHub Release (with a changelog and prebuilt binaries), and bumps
`Cargo.toml`.

So the version bump is decided entirely by the **commit messages** that land on
`main`. Format each commit (or squash-merge title) as:

```
<type>[optional scope][!]: <description>

[optional body]

[optional BREAKING CHANGE: footer]
```

### Which type to use → what release it triggers

The project is currently **pre-1.0.0**, which changes how breaking changes are
treated (they bump the *minor*, not the *major*, until 1.0.0 is cut).

| Commit type | Example | Release effect (pre-1.0.0) |
|-------------|---------|----------------------------|
| `feat:` | `feat: add ZRANDMEMBER command` | **minor** — `0.1.0 → 0.2.0` |
| `fix:` | `fix: correct LPOS RANK handling` | **patch** — `0.1.0 → 0.1.1` |
| `perf:` | `perf: avoid clone in GET path` | **patch** — `0.1.0 → 0.1.1` |
| `feat!:` / `fix!:` / `BREAKING CHANGE:` footer | `feat!: rename --bind flag` | **minor** while pre-1.0.0 (`0.1.0 → 0.2.0`); **major** once ≥ 1.0.0 |
| `docs:` `refactor:` `test:` `chore:` `ci:` `build:` `style:` | `chore: bump tokio` | **no release** (recorded, not published) |

Once the project reaches `1.0.0`, the mapping becomes standard semver:
`feat` → minor, `fix`/`perf` → patch, breaking → **major**.

### Rules of thumb

- Adding or extending a command → `feat:`.
- Fixing a wrong reply, wrong error text, or a crash → `fix:`.
- Removing/renaming a command, flag, or changing a reply shape in an
  incompatible way → mark it breaking with `!` and explain in a
  `BREAKING CHANGE:` footer.
- Docs, refactors, tests, CI, and dependency chores use their own types and
  will **not** cut a release on their own — batch them with a `feat`/`fix` or
  let them ride the next release.
- Keep the description imperative and lowercase: "add", not "Added"/"Adds".
- One logical change per commit; scope is optional (e.g. `feat(zset): ...`).

### Examples

```
feat(list): add LMPOP command
fix: return WRONGTYPE for APPEND on a list key
perf(resp): reuse the write buffer across replies
docs: document the --requirepass flag
feat!: make SELECT reject non-zero database indexes

BREAKING CHANGE: SELECT on any index other than 0 now errors instead of
being silently accepted.
```

## PR checklist

- `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`,
  and `cargo test` pass.
- `bash tests/compat/run.sh` passes (compatibility preserved / new cases added).
- The PR title / squashed commit follows the convention above so the release is
  versioned correctly.
