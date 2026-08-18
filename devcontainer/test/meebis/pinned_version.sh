#!/usr/bin/env bash
# The `version` option must install exactly what it names.
#
# Deliberately pinned to an older release than the current one: if the pin
# matched whatever `latest` resolves to, this test would pass without the
# option doing anything at all.
set -e

source dev-container-features-test-lib

check "installed the pinned version, not the newest" bash -c "meebis --version | grep -x 'meebis 0.11.0'"

reportResults
