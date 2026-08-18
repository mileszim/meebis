#!/usr/bin/env bash
# The other common devcontainer base, on the default `latest` version.
set -e

source dev-container-features-test-lib

check "meebis runs on an Ubuntu base" bash -c "meebis --version | grep -E '^meebis [0-9]+\.[0-9]+\.[0-9]+$'"
check "and serves a command" bash -c "meebis run -- sh -c 'echo \$REDIS_PORT' | grep -E '^[0-9]+$'"

reportResults
