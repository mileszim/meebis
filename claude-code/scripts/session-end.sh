#!/usr/bin/env bash
# SessionEnd: drop this session's claim on the project's meebis, and stop the
# server once nobody is left holding one.
#
# Reference counting rather than "whoever started it stops it": two Claude Code
# sessions in one project share the instance, and the first to exit must not
# pull it out from under the second.
set -uo pipefail

# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"

DIR="$(meebis_dir)"
[ -d "$DIR" ] || exit 0

INPUT="$(cat)"
SESSION_ID="$(hook_session_id "$INPUT")"
[ -n "$SESSION_ID" ] && rm -f "$DIR/sessions/$SESSION_ID"

take_lock
trap release_lock EXIT

# Someone else is still using it.
if [ -d "$DIR/sessions" ] && [ -n "$(ls -A "$DIR/sessions" 2>/dev/null)" ]; then
  exit 0
fi

PID="$(running_pid)" || {
  # Nothing of ours is running; clear whatever is left over.
  rm -f "$DIR/pid" "$DIR/port"
  exit 0
}

# SIGTERM is the signal meebis snapshots on, so a project that opted into
# `--dumpfile` through `.meebis/options` still gets its keyspace written.
kill -TERM "$PID" 2>/dev/null

for _ in $(seq 1 30); do
  is_meebis "$PID" || break
  sleep 0.1
done
is_meebis "$PID" && kill -KILL "$PID" 2>/dev/null

rm -f "$DIR/pid" "$DIR/port"
exit 0
