#!/usr/bin/env bash
# SessionStart: make sure this project has a meebis instance, and tell Claude
# where it is.
#
# Opt-in by directory: no `.meebis/` at the project root means this exits
# without doing anything, so the plugin is safe to install globally and only
# acts on projects that asked for it.
set -uo pipefail

# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"

DIR="$(meebis_dir)"
[ -d "$DIR" ] || exit 0

if ! command -v meebis >/dev/null 2>&1; then
  echo "meebis: $DIR exists but meebis is not on PATH — see https://github.com/mileszim/meebis#install" >&2
  exit 0
fi

INPUT="$(cat)"
SESSION_ID="$(hook_session_id "$INPUT")"

mkdir -p "$DIR/sessions" 2>/dev/null || exit 0
# Register this session before starting anything, so a server that does come up
# always has at least one owner and cannot be stopped by a racing SessionEnd.
[ -n "$SESSION_ID" ] && : >"$DIR/sessions/$SESSION_ID"

announce() {
  local port="$1"
  cat <<EOF
A disposable Redis (meebis) is running for this project on 127.0.0.1:$port.

- Connect with \`redis-cli -p $port\`, or REDIS_URL=redis://127.0.0.1:$port
- The port is OS-assigned and changes on every boot. Read it from
  \`.meebis/port\` rather than hard-coding it.
- The keyspace is in memory only and is discarded when the session ends. It is
  a scratch Redis, not a store of anything that matters.
EOF
}

take_lock
trap release_lock EXIT

# Already up — which is the common case on resume, compact, and any second
# session in the same project.
if PID="$(running_pid)"; then
  PORT="$(cat "$DIR/port" 2>/dev/null)"
  if [ -n "$PORT" ]; then
    announce "$PORT"
    exit 0
  fi
  # A live pid with no port is a half-written state from an interrupted start;
  # take it down and start cleanly rather than reason about it.
  kill -TERM "$PID" 2>/dev/null
fi

# A stale port file would be indistinguishable from a fresh one while the new
# server is still binding, so clear it and wait for meebis to write its own.
rm -f "$DIR/port" "$DIR/pid"

# `.meebis/options` is the escape hatch for a project that wants something
# other than the defaults — `--dumpfile .meebis/dump.rdb`, `--requirepass`,
# `--verbose`. Whitespace-separated; it is the project's own file.
OPTIONS=()
if [ -f "$DIR/options" ]; then
  # shellcheck disable=SC2207
  OPTIONS=($(tr '\n' ' ' <"$DIR/options"))
fi

nohup meebis --port 0 --port-file "$DIR/port" "${OPTIONS[@]+"${OPTIONS[@]}"}" \
  >>"$DIR/log" 2>&1 &
echo $! >"$DIR/pid"

# meebis writes the port file with a temp-file rename once it has bound, so a
# non-empty file means it is already accepting connections.
for _ in $(seq 1 50); do
  [ -s "$DIR/port" ] && break
  sleep 0.1
done

PORT="$(cat "$DIR/port" 2>/dev/null)"
if [ -z "$PORT" ]; then
  echo "meebis: the server did not report a port within 5s — see $DIR/log" >&2
  exit 0
fi

announce "$PORT"
