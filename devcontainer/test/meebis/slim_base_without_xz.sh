#!/usr/bin/env bash
# A slim base has neither curl nor xz. The feature is responsible for pulling in
# what it needs, and this is the scenario that proves it — the failure mode
# otherwise is a baffling tar error at image build time.
set -e

source dev-container-features-test-lib

check "meebis runs on a slim base" bash -c "meebis --version | grep -E '^meebis [0-9]+\.[0-9]+\.[0-9]+$'"

reportResults
