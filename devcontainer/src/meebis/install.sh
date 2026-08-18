#!/usr/bin/env bash
# Install the meebis binary into a devcontainer image.
#
# This runs as root during image build, once, with the feature's options in the
# environment (`VERSION` here, from devcontainer-feature.json). It installs a
# prebuilt release rather than building from source: meebis is a dev dependency,
# not the thing being developed, and a Rust toolchain is not worth adding to
# every image that wants a Redis.
#
# Nothing is started. meebis has no daemon, no data directory, and no config —
# a container that wants one running can add `meebis &` to postStartCommand, or
# better, wrap the command that needs it in `meebis run --`.
set -euo pipefail

VERSION="${VERSION:-latest}"
REPO="https://github.com/mileszim/meebis"
INSTALL_DIR="/usr/local/bin"

fail() {
  echo "meebis (feature): $*" >&2
  exit 1
}

[ "$(id -u)" = "0" ] || fail "this feature must run as root during image build"

# ---------------------------------------------------------------------------
# Dependencies. `xz` in particular is missing from plenty of slim base images,
# and its absence shows up as a baffling tar error rather than a clear one.
# ---------------------------------------------------------------------------

pkg_install() {
  if command -v apt-get >/dev/null 2>&1; then
    if [ -z "${_apt_updated:-}" ]; then
      apt-get update -y
      _apt_updated=1
    fi
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends "$@"
  elif command -v apk >/dev/null 2>&1; then
    apk add --no-cache "$@"
  elif command -v microdnf >/dev/null 2>&1; then
    microdnf install -y "$@"
  elif command -v dnf >/dev/null 2>&1; then
    dnf install -y "$@"
  elif command -v yum >/dev/null 2>&1; then
    yum install -y "$@"
  else
    fail "cannot install $* — no supported package manager found"
  fi
}

need() { command -v "$1" >/dev/null 2>&1; }

need curl || need wget || pkg_install curl ca-certificates
need tar || pkg_install tar
# GNU tar advertises `-J, --xz` in its help whether or not the `xz` binary it
# shells out to is installed, so the binary is the only thing worth testing
# for. Debian and Ubuntu call the package xz-utils; nearly everyone else calls
# it xz.
need xz || pkg_install xz-utils || pkg_install xz

fetch() {
  if need curl; then
    curl -fsSL --proto '=https' --tlsv1.2 -o "$2" "$1"
  else
    wget -qO "$2" "$1"
  fi
}

# ---------------------------------------------------------------------------
# Which build to fetch.
# ---------------------------------------------------------------------------

case "$(uname -m)" in
  x86_64 | amd64) arch="x86_64" ;;
  aarch64 | arm64) arch="aarch64" ;;
  *) fail "unsupported architecture $(uname -m) — meebis ships x86_64 and aarch64 Linux builds" ;;
esac
triple="${arch}-unknown-linux-gnu"

# The published Linux builds are dynamically linked against glibc 2.34 or
# newer. Both checks below exist to turn "cannot execute binary" — which says
# nothing about why — into a sentence naming the cause and a base image that
# works. The ghcr.io/mileszim/meebis image is statically linked and has neither
# constraint.
MIN_GLIBC="2.34"

for loader in /lib/ld-musl-*.so.1; do
  [ -e "$loader" ] || continue
  fail "this image uses musl (Alpine), and meebis publishes glibc builds only.
      Use a Debian or Ubuntu base, or the static image ghcr.io/mileszim/meebis."
done

glibc="$(ldd --version 2>/dev/null | head -1 | grep -o '[0-9]\+\.[0-9]\+$' || true)"
if [ -n "$glibc" ]; then
  # Two-field numeric compare: sort -V would also do, but is not everywhere.
  if [ "$(printf '%s\n%s\n' "$MIN_GLIBC" "$glibc" | sort -t. -k1,1n -k2,2n | head -1)" != "$MIN_GLIBC" ]; then
    fail "this image has glibc $glibc, and meebis needs $MIN_GLIBC or newer.
      Use a newer base (Ubuntu 22.04+, Debian 12+), or the static image
      ghcr.io/mileszim/meebis."
  fi
fi

# `latest` is resolved through the release download redirect rather than the
# API, which rate-limits unauthenticated callers to 60 an hour — a real risk in
# a CI image build.
if [ "$VERSION" = "latest" ]; then
  url="$REPO/releases/latest/download/meebis-${triple}.tar.xz"
else
  url="$REPO/releases/download/v${VERSION#v}/meebis-${triple}.tar.xz"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "meebis: downloading $url"
fetch "$url" "$tmp/meebis.tar.xz" ||
  fail "could not download meebis ${VERSION} for ${triple} — check that the version exists at $REPO/releases"

# Every archive has a checksum published beside it; there is no reason to bake
# an unverified binary into an image.
if fetch "$url.sha256" "$tmp/meebis.tar.xz.sha256" 2>/dev/null; then
  want="$(cut -d' ' -f1 <"$tmp/meebis.tar.xz.sha256")"
  got="$(sha256sum "$tmp/meebis.tar.xz" | cut -d' ' -f1)"
  [ "$want" = "$got" ] || fail "checksum mismatch (expected $want, got $got)"
  echo "meebis: checksum verified"
else
  echo "meebis: warning: no published checksum found — installing unverified" >&2
fi

# The archive wraps its contents in meebis-<triple>/.
tar -xJf "$tmp/meebis.tar.xz" -C "$tmp" --strip-components=1

install -m 0755 "$tmp/meebis" "$INSTALL_DIR/meebis"

# Fail the build rather than hand over an image whose meebis does not run.
"$INSTALL_DIR/meebis" --version >/dev/null 2>&1 ||
  fail "the installed binary does not run on this image"

echo "meebis: installed $("$INSTALL_DIR/meebis" --version) to $INSTALL_DIR/meebis"
