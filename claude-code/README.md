# meebis — Claude Code plugin

Gives a project its own disposable Redis for the length of a Claude Code
session, and teaches Claude how to use it.

## Install

```
/plugin marketplace add mileszim/meebis
/plugin install meebis@meebis
```

meebis itself has to be on `PATH` — see the [install
options](https://github.com/mileszim/meebis#install).

## What it does

**A skill** that tells Claude to reach for `meebis run -- <command>` when
something needs a Redis, where to find a running instance's port, how to trace
what an app is sending with `--verbose`, and — the part that saves the most
time — which Redis features meebis does not implement, so a missing `XREADGROUP`
is not mistaken for a bug in the code under test.

**Two hooks** that keep a per-project instance alive for the session.

## The hooks are opt-in per project

Installing the plugin does not start Redis servers all over your machine.
`SessionStart` does nothing unless the project root has a `.meebis/` directory,
so opting a project in is:

```sh
mkdir .meebis
echo .meebis/ >> .gitignore
```

From then on, every Claude Code session in that project gets an instance on an
OS-assigned port, and Claude is told where it is.

## What lives in `.meebis/`

| Path | |
|------|---|
| `port` | the listen port, written by meebis once it has bound |
| `pid` | the server process |
| `log` | the server's own output |
| `sessions/` | one file per live session, for reference counting |
| `options` | *(optional, yours)* extra flags, whitespace-separated |

`options` is the escape hatch when the defaults aren't right:

```sh
echo "--dumpfile .meebis/dump.rdb" > .meebis/options   # keep the keyspace
echo "--verbose" > .meebis/options                     # log every command
```

## Lifecycle details worth knowing

- **The port changes on every boot.** It is OS-assigned so that any number of
  projects and worktrees can run at once. Read `.meebis/port`.
- **Sessions share one instance.** Two Claude Code sessions in the same project
  get the same server; it is stopped when the *last* one ends, not the first.
- **A crashed session can leave a stale file** in `.meebis/sessions/`, which
  keeps the instance alive past its welcome. It costs about 2 MB. `rm -rf
  .meebis/sessions` and end a session to clear it.
- **The hooks never fail a session.** If meebis is missing, or the server does
  not come up, the hook says so on stderr and exits 0.

## What the hooks cannot do

They cannot put `REDIS_URL` into the environment of the tools Claude runs —
Claude Code hooks are child processes and there is no mechanism for one to set
session-wide environment variables. So the port is published to a file and
Claude is told to read it.

For anything that genuinely needs `REDIS_URL` in the environment, that is
exactly what `meebis run -- <command>` is for, and the skill points at it first.

## Tests

The hooks are shell, so nothing in the Rust build would notice them breaking.
`test-hooks.sh` drives them the way Claude Code does — the script named in
`hooks.json`, the event JSON on stdin — and checks the server it starts actually
answers:

```sh
cargo build --release
bash claude-code/test-hooks.sh
```

CI runs it on every pull request.
