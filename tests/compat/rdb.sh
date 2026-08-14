#!/usr/bin/env bash
#
# RDB interchange test: prove that meebis and a real redis-server can read each
# other's dump files.
#
# The RESP2 differential suite (run.sh) checks that the two servers *reply* the
# same. This checks something the fixtures cannot: that a snapshot written by
# one loads into the other with identical contents. Both directions matter and
# they exercise completely different code — meebis writes the flat encodings but
# has to read the listpacks, quicklists, and intsets Redis insists on emitting.
#
# Usage: bash tests/compat/rdb.sh [path-to-meebis-binary]
#
# Requires: redis-server, redis-cli on PATH.
set -uo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MEEBIS_BIN="${1:-$DIR/../../target/release/meebis}"
FIXTURE="$DIR/rdb-fixture.txt"
MPORT=6397
RPORT=6396
WORK="$(mktemp -d)"

if [[ ! -x "$MEEBIS_BIN" ]]; then
    echo "meebis binary not found/executable at: $MEEBIS_BIN" >&2
    exit 2
fi
for bin in redis-server redis-cli; do
    command -v "$bin" >/dev/null || { echo "missing required tool: $bin" >&2; exit 2; }
done

PIDS=()
cleanup() {
    for pid in "${PIDS[@]:-}"; do
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
    rm -rf "$WORK"
}
trap cleanup EXIT

wait_for() {
    local port=$1
    for _ in $(seq 1 100); do
        redis-cli -p "$port" ping >/dev/null 2>&1 && return 0
        sleep 0.1
    done
    return 1
}

# Canonical rendering of a server's entire keyspace: every database, every key,
# contents normalized so that orderings meebis does not promise to preserve
# (set members, hash fields) do not show up as differences.
dump_state() {
    local port=$1
    local db key type
    for db in 0 3 15; do
        while IFS= read -r key; do
            [[ -z "$key" ]] && continue
            type=$(redis-cli -n "$db" -p "$port" type "$key")
            echo "== db$db $key ($type)"
            case "$type" in
                string) redis-cli -n "$db" -p "$port" get "$key" ;;
                list)   redis-cli -n "$db" -p "$port" lrange "$key" 0 -1 ;;
                set)    redis-cli -n "$db" -p "$port" smembers "$key" | sort ;;
                hash)   redis-cli -n "$db" -p "$port" hgetall "$key" | paste - - | sort ;;
                zset)   redis-cli -n "$db" -p "$port" zrange "$key" 0 -1 withscores ;;
                stream) redis-cli -n "$db" -p "$port" xrange "$key" - + ;;
                *)      echo "UNKNOWN TYPE" ;;
            esac
            # Exact TTLs are timing-dependent; only whether one exists is not.
            if [[ "$(redis-cli -n "$db" -p "$port" ttl "$key")" == "-1" ]]; then
                echo "-- no ttl"
            else
                echo "-- has ttl"
            fi
        done < <(redis-cli -n "$db" -p "$port" --scan | sort)
    done
}

fail=0
report() {
    local name=$1 expected=$2 actual=$3
    if diff "$expected" "$actual" > "$WORK/diff.txt"; then
        echo "  ok   $name"
    else
        echo "  FAIL $name — source (<) vs reloaded (>):"
        sed 's/^/    /' "$WORK/diff.txt"
        fail=1
    fi
}

echo "== RDB interchange =="

# ---------------------------------------------------------------------------
# Direction 1: redis-server writes the dump, meebis loads it.
# ---------------------------------------------------------------------------
# An eviction policy is set deliberately: it makes Redis stamp LFU metadata
# between each key's expiry opcode and the key itself, which is the one arrangement
# that catches a loader dropping TTLs when an unrelated opcode intervenes.
# maxmemory is far above what the fixture needs, so nothing is actually evicted.
mkdir -p "$WORK/from-redis"
redis-server --port "$RPORT" --dir "$WORK/from-redis" --dbfilename dump.rdb \
    --save '' --appendonly no --logfile /dev/null \
    --maxmemory 100mb --maxmemory-policy allkeys-lfu &
PIDS+=($!)
wait_for "$RPORT" || { echo "reference redis did not start" >&2; exit 2; }

redis-cli -p "$RPORT" flushall >/dev/null
redis-cli -p "$RPORT" < "$FIXTURE" >/dev/null
# Push two collections past their listpack thresholds so the dump contains the
# encodings Redis only uses at size: a real quicklist and a hashtable hash.
redis-cli -p "$RPORT" rpush biglist $(seq 1 400 | sed 's/^/item-000000000000000000000000000000000000000000000000000000000000000000000000000000-/') >/dev/null
redis-cli -p "$RPORT" hset bighash $(seq 1 600 | sed 's/^/f-/;s/$/ v/') >/dev/null
redis-cli -p "$RPORT" sadd bigset $(seq 1 300 | sed 's/^/member-/') >/dev/null
redis-cli -p "$RPORT" zadd bigzset $(seq 1 300 | sed 's/^/1 m-/') >/dev/null
redis-cli -p "$RPORT" save >/dev/null

dump_state "$RPORT" > "$WORK/redis-state.txt"

"$MEEBIS_BIN" --port "$MPORT" --dumpfile "$WORK/from-redis/dump.rdb" --dumpfile-strict \
    > "$WORK/meebis-load.log" 2>&1 &
PIDS+=($!)
if ! wait_for "$MPORT"; then
    echo "  FAIL meebis refused to start on a redis-written dump:"
    sed 's/^/    /' "$WORK/meebis-load.log"
    exit 1
fi
dump_state "$MPORT" > "$WORK/meebis-state.txt"
report "redis-server dump -> meebis" "$WORK/redis-state.txt" "$WORK/meebis-state.txt"

redis-cli -p "$MPORT" shutdown nosave 2>/dev/null || true
redis-cli -p "$RPORT" shutdown nosave 2>/dev/null || true
sleep 0.3

# ---------------------------------------------------------------------------
# Direction 2: meebis writes the dump, redis-server loads it.
# ---------------------------------------------------------------------------
mkdir -p "$WORK/from-meebis"
"$MEEBIS_BIN" --port "$MPORT" --dumpfile "$WORK/from-meebis/dump.rdb" \
    > "$WORK/meebis-save.log" 2>&1 &
PIDS+=($!)
wait_for "$MPORT" || { echo "meebis did not start" >&2; exit 2; }

redis-cli -p "$MPORT" flushall >/dev/null
redis-cli -p "$MPORT" < "$FIXTURE" >/dev/null
redis-cli -p "$MPORT" rpush biglist $(seq 1 400 | sed 's/^/item-/') >/dev/null
redis-cli -p "$MPORT" hset bighash $(seq 1 600 | sed 's/^/f-/;s/$/ v/') >/dev/null

dump_state "$MPORT" > "$WORK/meebis-source.txt"
redis-cli -p "$MPORT" save >/dev/null

redis-server --port "$RPORT" --dir "$WORK/from-meebis" --dbfilename dump.rdb \
    --save '' --appendonly no --logfile "$WORK/redis-load.log" &
PIDS+=($!)
if ! wait_for "$RPORT"; then
    echo "  FAIL redis-server refused to load a meebis-written dump:"
    sed 's/^/    /' "$WORK/redis-load.log"
    exit 1
fi
dump_state "$RPORT" > "$WORK/redis-reloaded.txt"
report "meebis dump -> redis-server" "$WORK/meebis-source.txt" "$WORK/redis-reloaded.txt"

# Redis logs a warning rather than failing for a merely odd file, so make sure
# it did not quietly discard anything.
if grep -qiE "corrupt|error|wrong signature|bad|failed" "$WORK/redis-load.log"; then
    echo "  FAIL redis-server complained while loading the meebis dump:"
    sed 's/^/    /' "$WORK/redis-load.log"
    fail=1
fi

# ---------------------------------------------------------------------------
# Direction 3: meebis reloads its own dump across a restart.
# ---------------------------------------------------------------------------
redis-cli -p "$MPORT" shutdown 2>/dev/null || true
sleep 0.3
"$MEEBIS_BIN" --port "$MPORT" --dumpfile "$WORK/from-meebis/dump.rdb" --dumpfile-strict \
    > "$WORK/meebis-restart.log" 2>&1 &
PIDS+=($!)
if ! wait_for "$MPORT"; then
    echo "  FAIL meebis refused to restart on its own dump:"
    sed 's/^/    /' "$WORK/meebis-restart.log"
    exit 1
fi
dump_state "$MPORT" > "$WORK/meebis-restarted.txt"
report "meebis dump -> meebis (restart)" "$WORK/meebis-source.txt" "$WORK/meebis-restarted.txt"

if [[ $fail -eq 0 ]]; then
    echo "RDB interchange: all directions agree"
else
    echo "RDB interchange: FAILURES"
fi
exit $fail
