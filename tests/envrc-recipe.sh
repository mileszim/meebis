#!/usr/bin/env bash
# Run the direnv recipe exactly as the README prints it.
#
#   bash tests/envrc-recipe.sh [path/to/meebis]
#
# The snippet is extracted from README.md rather than kept beside this script,
# so there is no second copy to drift: if someone edits the documented recipe
# into something that does not work, this fails.
set -uo pipefail

HERE="$(cd "$(dirname "$0")/.." && pwd)"
MEEBIS="${1:-$HERE/target/release/meebis}"
README="$HERE/README.md"
MARKER="<!-- envrc-recipe -->"

[ -x "$MEEBIS" ] || {
  echo "no meebis binary at $MEEBIS (cargo build --release first)" >&2
  exit 1
}
PATH="$(cd "$(dirname "$MEEBIS")" && pwd):$PATH"
export PATH

fail_count=0
ok() { echo "  ok   $1"; }
bad() {
  echo "  FAIL $1"
  fail_count=$((fail_count + 1))
}

WORK="$(mktemp -d)"
cleanup() {
  [ -s "$WORK/project/.meebis/pid" ] && kill "$(cat "$WORK/project/.meebis/pid")" 2>/dev/null
  rm -rf "$WORK"
}
trap cleanup EXIT

echo "== 0. extract the recipe from the README =="
# The first fenced block after the marker.
awk -v marker="$MARKER" '
  index($0, marker) { found = 1; next }
  found && /^```/ { infence = !infence; if (!infence) exit; next }
  found && infence { print }
' "$README" >"$WORK/envrc"

if [ -s "$WORK/envrc" ]; then
  ok "found the snippet ($(wc -l <"$WORK/envrc" | tr -d ' ') lines)"
else
  bad "no snippet after '$MARKER' in README.md"
  echo "SOME ENVRC RECIPE TESTS FAILED"
  exit 1
fi

mkdir -p "$WORK/project"

# direnv evaluates .envrc under `set -euo pipefail`, which is the strictest
# thing the snippet has to survive — an unguarded failing command would abort
# the load and leave the shell with no variables at all.
load() {
  (
    cd "$WORK/project" || exit 1
    set -euo pipefail
    # shellcheck disable=SC1090
    . "$WORK/envrc"
    echo "REDIS_URL=${REDIS_URL-}"
    echo "REDIS_HOST=${REDIS_HOST-}"
    echo "REDIS_PORT=${REDIS_PORT-}"
  ) 2>"$WORK/stderr"
}

echo "== 1. a first load starts a server and exports it =="
first="$(load)"
rc=$?
if [ "$rc" = 0 ]; then ok "the snippet ran clean under set -euo pipefail"; else bad "exited $rc: $(cat "$WORK/stderr")"; fi

url="$(echo "$first" | sed -n 's/^REDIS_URL=//p')"
port="$(echo "$first" | sed -n 's/^REDIS_PORT=//p')"
host="$(echo "$first" | sed -n 's/^REDIS_HOST=//p')"
case "$url" in
  redis://127.0.0.1:[0-9]*) ok "exported $url" ;;
  *) bad "REDIS_URL is '$url'" ;;
esac
if [ "$host" = "127.0.0.1" ]; then ok "exported REDIS_HOST"; else bad "REDIS_HOST is '$host'"; fi
if [ -n "$port" ] && [ "$port" != "0" ]; then ok "exported a resolved port ($port)"; else bad "REDIS_PORT is '$port'"; fi

echo "== 2. the server it started actually answers =="
reply="$(
  exec 3<>"/dev/tcp/127.0.0.1/$port" 2>/dev/null &&
    printf 'PING\r\n' >&3 &&
    head -c 7 <&3
)"
if [ "$reply" = "$(printf '+PONG\r\n')" ]; then ok "PING → +PONG"; else bad "got '$reply'"; fi

echo "== 3. reloading reuses it instead of starting another =="
pid_before="$(cat "$WORK/project/.meebis/pid")"
second="$(load)"
port_again="$(echo "$second" | sed -n 's/^REDIS_PORT=//p')"
pid_after="$(cat "$WORK/project/.meebis/pid")"
if [ "$port_again" = "$port" ]; then ok "same port on reload ($port)"; else bad "port changed from $port to $port_again"; fi
if [ "$pid_after" = "$pid_before" ]; then ok "same process"; else bad "started a second server ($pid_before → $pid_after)"; fi
# Three loads in a row is the case direnv actually produces, one per `cd`.
load >/dev/null
count="$(pgrep -f "port-file $WORK/project/.meebis/port" 2>/dev/null | wc -l | tr -d ' ')"
if [ "${count:-1}" -le 1 ]; then ok "still exactly one server after three loads"; else bad "$count servers running"; fi

echo "== 4. a dead server is replaced, not inherited =="
kill "$pid_before" 2>/dev/null
for _ in $(seq 1 30); do kill -0 "$pid_before" 2>/dev/null || break; sleep 0.1; done
third="$(load)"
port_new="$(echo "$third" | sed -n 's/^REDIS_PORT=//p')"
pid_new="$(cat "$WORK/project/.meebis/pid")"
if [ "$pid_new" != "$pid_before" ]; then ok "started a fresh server"; else bad "reused a dead pid"; fi
if [ -n "$port_new" ] && [ "$port_new" != "0" ]; then ok "exported the new port ($port_new)"; else bad "REDIS_PORT is '$port_new'"; fi
reply="$(
  exec 3<>"/dev/tcp/127.0.0.1/$port_new" 2>/dev/null &&
    printf 'PING\r\n' >&3 &&
    head -c 7 <&3
)"
if [ "$reply" = "$(printf '+PONG\r\n')" ]; then ok "the replacement answers"; else bad "got '$reply'"; fi

echo "== 5. a missing meebis reports rather than exporting a broken URL =="
out="$(
  cd "$WORK" && mkdir -p empty && cd empty || exit 1
  PATH=/usr/bin:/bin bash -c "set -euo pipefail; . '$WORK/envrc'; echo \"REDIS_URL=\${REDIS_URL-unset}\"" 2>"$WORK/stderr2"
)"
case "$out" in
  *"REDIS_URL=unset"*) ok "exported nothing" ;;
  *) bad "exported '$out' with no meebis on PATH" ;;
esac
if grep -q "did not come up" "$WORK/stderr2"; then ok "said so on stderr"; else bad "silent: $(cat "$WORK/stderr2")"; fi

echo
if [ "$fail_count" = 0 ]; then
  echo "ALL ENVRC RECIPE TESTS PASSED"
else
  echo "SOME ENVRC RECIPE TESTS FAILED"
fi
exit "$fail_count"
