#!/usr/bin/env bash
#
# Redis compatibility test: run each fixture of commands through both meebis
# and a reference redis-server and assert the replies are byte-for-byte
# identical (RESP2, via redis-cli), then run the RESP3 parity check (via
# redis-py) if it is available.
#
# Usage: bash tests/compat/run.sh [path-to-meebis-binary]
#
# Requires: redis-server, redis-cli on PATH. Optionally python3 with the
# `redis` package for the RESP3 stage.
set -uo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MEEBIS_BIN="${1:-$DIR/../../target/release/meebis}"
MPORT=6399
RPORT=6398

if [[ ! -x "$MEEBIS_BIN" ]]; then
    echo "meebis binary not found/executable at: $MEEBIS_BIN" >&2
    echo "build it first: cargo build --release" >&2
    exit 2
fi
for bin in redis-server redis-cli; do
    command -v "$bin" >/dev/null || { echo "missing required tool: $bin" >&2; exit 2; }
done

redis-server --port "$RPORT" --save '' --appendonly no --logfile /dev/null &
RPID=$!
"$MEEBIS_BIN" --port "$MPORT" >/dev/null 2>&1 &
MPID=$!
cleanup() {
    kill "$MPID" "$RPID" 2>/dev/null || true
    wait "$MPID" "$RPID" 2>/dev/null || true
}
trap cleanup EXIT

# Wait for both servers to accept connections.
for _ in $(seq 1 100); do
    if redis-cli -p "$MPORT" ping >/dev/null 2>&1 && redis-cli -p "$RPORT" ping >/dev/null 2>&1; then
        break
    fi
    sleep 0.1
done
redis-cli -p "$MPORT" ping >/dev/null 2>&1 || { echo "meebis did not start" >&2; exit 2; }
redis-cli -p "$RPORT" ping >/dev/null 2>&1 || { echo "reference redis did not start" >&2; exit 2; }

fail=0

echo "== RESP2 differential (redis-cli) =="
for f in "$DIR"/resp2/*.txt; do
    name="$(basename "$f")"
    redis-cli -p "$MPORT" flushall >/dev/null
    redis-cli -p "$RPORT" flushall >/dev/null
    if diff <(redis-cli -p "$MPORT" < "$f") <(redis-cli -p "$RPORT" < "$f") >/tmp/meebis-diff.txt; then
        echo "  ok   $name"
    else
        echo "  FAIL $name — meebis (<) vs redis (>):"
        sed 's/^/    /' /tmp/meebis-diff.txt
        fail=1
    fi
done

echo "== RESP3 parity (redis-py) =="
if command -v python3 >/dev/null 2>&1 && python3 -c 'import redis' >/dev/null 2>&1; then
    if python3 "$DIR/resp3_parity.py" "$MPORT" "$RPORT"; then
        echo "  ok   RESP3 parity"
    else
        fail=1
    fi
else
    echo "  skipped (python3 + redis package not available)"
fi

if [[ "$fail" -eq 0 ]]; then
    echo "ALL COMPATIBILITY TESTS PASSED"
else
    echo "COMPATIBILITY TESTS FAILED" >&2
fi
exit "$fail"
