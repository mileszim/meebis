#!/usr/bin/env bash
# Default-options test: the feature is applied to the image in
# devcontainer-feature.json's default configuration, and this runs inside it.
set -e

source dev-container-features-test-lib

check "meebis is on PATH" bash -c "command -v meebis"
check "meebis runs" bash -c "meebis --version | grep -E '^meebis [0-9]+\.[0-9]+\.[0-9]+$'"

# The feature installs a binary and starts nothing, so the useful proof is that
# a server can be started on demand and actually speaks RESP.
check "serves RESP on demand" bash -c '
  meebis --port 0 --port-file /tmp/p >/tmp/log 2>&1 &
  for _ in $(seq 1 50); do [ -s /tmp/p ] && break; sleep 0.1; done
  port=$(cat /tmp/p)
  exec 3<>/dev/tcp/127.0.0.1/$port
  printf "PING\r\n" >&3
  head -c 7 <&3 | grep -q PONG
'

# `meebis run --` is the shape a devcontainer usually wants: no daemon, no
# lifecycle to manage in the container config.
check "meebis run passes REDIS_URL to the command" bash -c '
  meebis run -- sh -c "echo \$REDIS_URL" | grep -E "^redis://127\.0\.0\.1:[0-9]+$"
'

reportResults
