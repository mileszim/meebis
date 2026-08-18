#!/usr/bin/env bash
# End-to-end exercise of the session hooks, driving them exactly as Claude Code
# does: the script on the path in hooks.json, the event JSON on stdin.
#
#   bash claude-code/test-hooks.sh [path/to/meebis]
#
# Needs `redis-cli` on PATH to prove the server it starts is a real one. The
# default binary is ./target/release/meebis.
set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
START="$HERE/scripts/session-start.sh"
END="$HERE/scripts/session-end.sh"
MEEBIS="${1:-$HERE/../target/release/meebis}"

if [ ! -x "$MEEBIS" ]; then
  echo "no meebis binary at $MEEBIS (cargo build --release first)" >&2
  exit 1
fi
if ! command -v redis-cli >/dev/null 2>&1; then
  echo "redis-cli is needed to check the server actually answers" >&2
  exit 1
fi
# Put the binary under test first on PATH, which is how the hooks find it.
PATH="$(cd "$(dirname "$MEEBIS")" && pwd):$PATH"
export PATH

fail=0
ok() { echo "  ok   $1"; }
bad() {
  echo "  FAIL $1"
  fail=1
}
check() { if [ "$2" = "$3" ]; then ok "$1"; else bad "$1 (got '$2', want '$3')"; fi; }

# Feed a hook the event JSON it expects on stdin.
hook() {
  echo "{\"session_id\":\"$2\",\"cwd\":\"$CLAUDE_PROJECT_DIR\",\"hook_event_name\":\"x\"}" | "$1"
}

echo "== 1. a project that has not opted in gets nothing =="
P=$(mktemp -d)
export CLAUDE_PROJECT_DIR="$P"
out=$(hook "$START" s1 2>&1)
rc=$?
check "exits 0" "$rc" "0"
check "says nothing" "$out" ""
check "starts nothing" "$(ls -A "$P")" ""
rm -rf "$P"

echo "== 2. an opted-in project gets a server =="
P=$(mktemp -d)
export CLAUDE_PROJECT_DIR="$P"
mkdir "$P/.meebis"
out=$(hook "$START" sA)
rc=$?
check "exits 0" "$rc" "0"
PORT=$(cat "$P/.meebis/port" 2>/dev/null)
PID=$(cat "$P/.meebis/pid" 2>/dev/null)
if [ -n "$PORT" ]; then ok "wrote a port ($PORT)"; else bad "no port"; fi
case "$out" in
  *"127.0.0.1:$PORT"*) ok "announced the port" ;;
  *) bad "announcement missing the port" ;;
esac
if redis-cli -p "$PORT" ping 2>/dev/null | grep -q PONG; then
  ok "server answers PING"
else bad "server unreachable"; fi
check "registered the session" "$(ls "$P/.meebis/sessions")" "sA"

echo "== 3. a second session shares it =="
out=$(hook "$START" sB)
check "same pid" "$(cat "$P/.meebis/pid")" "$PID"
check "same port" "$(cat "$P/.meebis/port")" "$PORT"
check "both registered" "$(ls "$P/.meebis/sessions" | tr '\n' ' ')" "sA sB "
case "$out" in
  *"127.0.0.1:$PORT"*) ok "re-announced to the second session" ;;
  *) bad "no announcement" ;;
esac

echo "== 4. the first to leave does not take it away =="
hook "$END" sA >/dev/null
check "the other is still registered" "$(ls "$P/.meebis/sessions")" "sB"
if kill -0 "$PID" 2>/dev/null; then ok "server still running"; else bad "stopped too early"; fi
if redis-cli -p "$PORT" ping 2>/dev/null | grep -q PONG; then
  ok "still answers"
else bad "unreachable"; fi

echo "== 5. the last to leave stops it =="
hook "$END" sB >/dev/null
sleep 0.5
if kill -0 "$PID" 2>/dev/null; then bad "server outlived the last session"; else ok "server stopped"; fi
if [ -f "$P/.meebis/pid" ]; then bad "pid file left behind"; else ok "pid file cleared"; fi
if [ -f "$P/.meebis/port" ]; then bad "port file left behind"; else ok "port file cleared"; fi
rm -rf "$P"

echo "== 6. .meebis/options reaches the server =="
P=$(mktemp -d)
export CLAUDE_PROJECT_DIR="$P"
mkdir "$P/.meebis"
echo "--requirepass hunter2" >"$P/.meebis/options"
hook "$START" sC >/dev/null
PORT=$(cat "$P/.meebis/port")
if redis-cli -p "$PORT" ping 2>&1 | grep -qi "NOAUTH"; then
  ok "the option took effect"
else bad "options were ignored"; fi
if redis-cli -p "$PORT" -a hunter2 --no-auth-warning ping 2>/dev/null | grep -q PONG; then
  ok "and the password works"
else bad "auth broken"; fi
hook "$END" sC >/dev/null
sleep 0.3
rm -rf "$P"

echo "== 7. a stale pid/port from a killed run is replaced =="
P=$(mktemp -d)
export CLAUDE_PROJECT_DIR="$P"
mkdir "$P/.meebis"
echo "999999" >"$P/.meebis/pid"
echo "1234" >"$P/.meebis/port"
hook "$START" sD >/dev/null
PORT=$(cat "$P/.meebis/port")
if [ "$PORT" != "1234" ]; then ok "replaced the stale port ($PORT)"; else bad "trusted a stale port"; fi
if redis-cli -p "$PORT" ping 2>/dev/null | grep -q PONG; then
  ok "the new server answers"
else bad "unreachable"; fi
hook "$END" sD >/dev/null
sleep 0.3
rm -rf "$P"

echo "== 8. a missing meebis is reported, not fatal =="
P=$(mktemp -d)
export CLAUDE_PROJECT_DIR="$P"
mkdir "$P/.meebis"
# A PATH with a shell but no meebis on it.
err=$(PATH=/usr/bin:/bin hook "$START" sE 2>&1 >/dev/null)
rc=$?
check "exits 0" "$rc" "0"
case "$err" in
  *"not on PATH"*) ok "explains itself" ;;
  *) bad "unhelpful message: '$err'" ;;
esac
rm -rf "$P"

echo
if [ "$fail" = 0 ]; then
  echo "ALL HOOK TESTS PASSED"
else
  echo "SOME HOOK TESTS FAILED"
fi
exit "$fail"
