#!/usr/bin/env bash
# Exercise the asdf plugin in bin/ the way asdf drives it: the ASDF_* variables
# in the environment, one script per phase.
#
#   bash tests/asdf-plugin.sh
#
# This talks to GitHub, because installing a published release is the entire
# job. It is pinned to an old release rather than the newest one so that a
# release still uploading its assets cannot make it flake.
set -uo pipefail

HERE="$(cd "$(dirname "$0")/.." && pwd)"
PLUGIN="$HERE/bin"
# Any release old enough that its assets are certainly in place.
VERSION="0.11.0"

fail_count=0
ok() { echo "  ok   $1"; }
bad() {
  echo "  FAIL $1"
  fail_count=$((fail_count + 1))
}

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

echo "== 1. list-all reports releases, oldest first =="
versions="$("$PLUGIN/list-all")"
rc=$?
if [ "$rc" = 0 ] && [ -n "$versions" ]; then ok "returned versions"; else bad "list-all failed"; fi
case " $versions " in
  *" $VERSION "*) ok "includes $VERSION" ;;
  *) bad "missing $VERSION in: $versions" ;;
esac
# The ordering is the point: a lexical sort would put 0.10.0 before 0.9.0.
if [ -n "$(echo "$versions" | grep -o '0\.9\.0 .*0\.10\.0')" ]; then
  ok "0.9.0 sorts before 0.10.0"
elif ! echo "$versions" | grep -q '0\.10\.0'; then
  ok "0.10.0 not released yet, ordering not exercised"
else
  bad "version ordering is lexical, not numeric: $versions"
fi

echo "== 2. latest-stable is the newest of them =="
latest="$("$PLUGIN/latest-stable")"
expected="$(echo "$versions" | tr ' ' '\n' | grep -v '^$' | tail -n 1)"
if [ "$latest" = "$expected" ]; then ok "latest-stable is $latest"; else bad "latest-stable said '$latest', list ends at '$expected'"; fi

echo "== 3. download fetches and verifies a release =="
export ASDF_INSTALL_TYPE=version
export ASDF_INSTALL_VERSION="$VERSION"
export ASDF_DOWNLOAD_PATH="$WORK/download"
export ASDF_INSTALL_PATH="$WORK/install"
mkdir -p "$ASDF_DOWNLOAD_PATH" "$ASDF_INSTALL_PATH"

if "$PLUGIN/download" >"$WORK/download.log" 2>&1; then
  ok "downloaded v$VERSION"
else
  bad "download failed: $(tail -2 "$WORK/download.log")"
fi
if [ -f "$ASDF_DOWNLOAD_PATH/meebis" ]; then ok "extracted the binary"; else bad "no binary in the download path"; fi
# A missing checksum is tolerated but must be announced; a present one must not
# warn. Either way the word to look for is the same.
if grep -q "installing unverified" "$WORK/download.log"; then
  bad "the release had no published checksum to verify against"
else
  ok "verified the published checksum"
fi

echo "== 4. install places a working binary =="
if "$PLUGIN/install" >"$WORK/install.log" 2>&1; then ok "install succeeded"; else bad "install failed: $(tail -2 "$WORK/install.log")"; fi
if [ -x "$ASDF_INSTALL_PATH/bin/meebis" ]; then ok "binary is executable"; else bad "no executable at bin/meebis"; fi
reported="$("$ASDF_INSTALL_PATH/bin/meebis" --version 2>/dev/null)"
if [ "$reported" = "meebis $VERSION" ]; then ok "runs and reports $reported"; else bad "reported '$reported'"; fi

echo "== 5. a version that does not exist fails clearly =="
out="$(ASDF_INSTALL_VERSION=99.99.99 ASDF_DOWNLOAD_PATH="$WORK/nope" "$PLUGIN/download" 2>&1)"
rc=$?
if [ "$rc" != 0 ]; then ok "exits non-zero"; else bad "reported success for a missing version"; fi
case "$out" in
  *"could not download"*) ok "explains itself" ;;
  *) bad "unhelpful message: $out" ;;
esac

echo "== 6. a ref install is refused rather than half-done =="
out="$(ASDF_INSTALL_TYPE=ref ASDF_INSTALL_VERSION=main "$PLUGIN/download" 2>&1)"
rc=$?
if [ "$rc" != 0 ]; then ok "exits non-zero"; else bad "accepted a ref install"; fi
case "$out" in
  *"only released versions"*) ok "explains itself" ;;
  *) bad "unhelpful message: $out" ;;
esac

echo "== 7. install refuses a binary that disagrees with the version =="
# The download path still holds $VERSION; asking to install a different one
# must be caught rather than recorded under the wrong name.
out="$(ASDF_INSTALL_VERSION=0.1.0 ASDF_INSTALL_PATH="$WORK/mismatch" "$PLUGIN/install" 2>&1)"
rc=$?
if [ "$rc" != 0 ]; then ok "exits non-zero"; else bad "installed a mismatched version"; fi
if [ -d "$WORK/mismatch/bin" ]; then bad "left a half-written install behind"; else ok "wrote nothing"; fi

echo
if [ "$fail_count" = 0 ]; then
  echo "ALL ASDF PLUGIN TESTS PASSED"
else
  echo "SOME ASDF PLUGIN TESTS FAILED"
fi
exit "$fail_count"
