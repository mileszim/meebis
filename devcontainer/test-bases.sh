#!/usr/bin/env bash
# Check install.sh against base images it must support — and, just as
# importantly, against ones it cannot.
#
#   bash devcontainer/test-bases.sh
#
# `devcontainer features test` cannot cover the second half: a feature that
# fails to install fails the image build, so the harness has no way to assert
# that it failed *well*. An unsupported base is a thing people will hit, and
# "cannot execute binary file" tells them nothing, so the message is worth a
# test of its own.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
FEATURE="$HERE/src/meebis"

command -v docker >/dev/null 2>&1 || {
  echo "docker is required" >&2
  exit 1
}

fail_count=0
ok() { echo "  ok   $1"; }
bad() {
  echo "  FAIL $1"
  fail_count=$((fail_count + 1))
}

# Run install.sh inside `image` and echo "<exit code>\t<last meebis message>".
attempt() {
  docker run --rm -v "$FEATURE:/f:ro" "$1" sh -c '
    command -v bash >/dev/null 2>&1 || apk add --no-cache bash >/dev/null 2>&1
    bash /f/install.sh >/tmp/out 2>&1
    rc=$?
    if [ $rc = 0 ]; then
      printf "0\t%s\n" "$(meebis --version 2>&1)"
    else
      printf "%s\t%s\n" "$rc" "$(grep "meebis (feature)" /tmp/out | head -1)"
    fi
  ' 2>/dev/null | tail -1
}

echo "== supported bases install a working meebis =="
for image in debian:bookworm-slim ubuntu:22.04 mcr.microsoft.com/devcontainers/base:debian; do
  result="$(attempt "$image")"
  code="${result%%	*}"
  detail="${result#*	}"
  case "$code:$detail" in
    "0:meebis "*) ok "$image → $detail" ;;
    *) bad "$image → exit $code: $detail" ;;
  esac
done

echo "== unsupported bases are refused with a reason =="
# glibc older than the 2.34 the release binaries are linked against.
for image in ubuntu:focal debian:bullseye-slim; do
  result="$(attempt "$image")"
  code="${result%%	*}"
  detail="${result#*	}"
  if [ "$code" = "0" ]; then
    bad "$image installed, but its glibc is too old to run the binary"
  else
    case "$detail" in
      *"glibc"*"or newer"*) ok "$image → refused: ${detail#meebis (feature): }" ;;
      *) bad "$image failed without naming glibc: $detail" ;;
    esac
  fi
done

# musl has no glibc version to compare at all, so it needs its own check.
result="$(attempt alpine:3.20)"
code="${result%%	*}"
detail="${result#*	}"
if [ "$code" = "0" ]; then
  bad "alpine installed, but the glibc binary cannot run on musl"
else
  case "$detail" in
    *musl*) ok "alpine:3.20 → refused: ${detail#meebis (feature): }" ;;
    *) bad "alpine failed without naming musl: $detail" ;;
  esac
fi

echo
if [ "$fail_count" = 0 ]; then
  echo "ALL BASE IMAGE TESTS PASSED"
else
  echo "SOME BASE IMAGE TESTS FAILED"
fi
exit "$fail_count"
