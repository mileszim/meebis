#!/usr/bin/env bash
# Shared helpers for the asdf plugin in this directory.
#
# meebis hosts its own asdf plugin rather than living in a separate
# `asdf-meebis` repo: asdf clones a plugin from a git URL and looks for `bin/`
# at its root, so this is what lets
#
#     asdf plugin add meebis https://github.com/mileszim/meebis.git
#
# work against the tool's own repository. mise reaches the same scripts through
# its `asdf:` backend.

set -euo pipefail

REPO="https://github.com/mileszim/meebis"

fail() {
  echo "meebis (asdf): $*" >&2
  exit 1
}

# The Rust target triple naming the release asset for this machine. Releases
# carry all four combinations, so anything else is genuinely unsupported rather
# than merely missing.
target_triple() {
  local os arch
  case "$(uname -s)" in
    Darwin) os="apple-darwin" ;;
    Linux) os="unknown-linux-gnu" ;;
    *) fail "unsupported operating system $(uname -s) — meebis ships macOS and Linux builds" ;;
  esac
  case "$(uname -m)" in
    arm64 | aarch64) arch="aarch64" ;;
    x86_64 | amd64) arch="x86_64" ;;
    *) fail "unsupported architecture $(uname -m) — meebis ships arm64 and x86_64 builds" ;;
  esac
  printf '%s-%s' "$arch" "$os"
}

# Whichever downloader is present. Both are near-universal, but neither is
# guaranteed, and failing here with a clear reason beats a confusing one later.
fetch() {
  local url="$1" out="$2"
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --proto '=https' --tlsv1.2 -o "$out" "$url"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO "$out" "$url"
  else
    fail "neither curl nor wget is available to download $url"
  fi
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d' ' -f1
  else
    fail "neither sha256sum nor shasum is available to verify the download"
  fi
}

# Newest-last ordering that understands that 0.10.0 follows 0.9.0, which a
# plain lexical sort does not.
sort_versions() {
  sort -t. -k1,1n -k2,2n -k3,3n
}
