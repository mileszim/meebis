#!/usr/bin/env bash
# Shared helpers for the meebis session hooks.
#
# Everything a hook needs to know lives in one directory, `.meebis/` at the
# project root:
#
#   .meebis/            its existence is the opt-in — no directory, no server
#   .meebis/port        the listen port, written by meebis once it has bound
#   .meebis/pid         the server process
#   .meebis/options     optional extra flags, whitespace-separated
#   .meebis/sessions/   one empty file per live session, for reference counting
#   .meebis/log         the server's own output
#
# Hooks must never be the reason a session fails to start, so every path here
# ends in `exit 0`; problems are reported on stderr and the session carries on
# without a Redis.

# The project's meebis directory. `CLAUDE_PROJECT_DIR` is set for hooks; the
# fallback is only for running these scripts by hand.
meebis_dir() {
  printf '%s/.meebis' "${CLAUDE_PROJECT_DIR:-$PWD}"
}

# Pull `session_id` out of the JSON a hook receives on stdin. `jq` is not a
# given on a developer's machine, so fall back to a plain text match — the
# field is a flat string and Claude Code does not escape anything inside it.
hook_session_id() {
  local input="$1"
  if command -v jq >/dev/null 2>&1; then
    printf '%s' "$input" | jq -r '.session_id // empty' 2>/dev/null
    return 0
  fi
  printf '%s' "$input" |
    sed -n 's/.*"session_id"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' |
    head -n 1
}

# Whether `pid` is a live meebis. Checking the name as well as the pid matters
# on the stop path: pids get recycled, and a hook that kills whatever inherited
# the number would be far worse than one that leaves a server running.
is_meebis() {
  local pid="$1"
  [ -n "$pid" ] || return 1
  kill -0 "$pid" 2>/dev/null || return 1
  ps -p "$pid" -o comm= 2>/dev/null | grep -q 'meebis'
}

# The pid recorded for this project, if it is still a live meebis.
running_pid() {
  local dir pid
  dir="$(meebis_dir)"
  [ -s "$dir/pid" ] || return 1
  pid="$(cat "$dir/pid" 2>/dev/null)" || return 1
  is_meebis "$pid" || return 1
  printf '%s' "$pid"
}

# Take a lock so two sessions starting at once cannot each boot a server.
# `mkdir` is the atomic primitive available in every shell. A lock older than
# the startup timeout is assumed to belong to a hook that was killed, and is
# broken rather than waited on forever.
take_lock() {
  local dir lock i=0
  dir="$(meebis_dir)"
  lock="$dir/.lock"
  while ! mkdir "$lock" 2>/dev/null; do
    i=$((i + 1))
    if [ "$i" -gt 100 ]; then
      rm -rf "$lock"
      continue
    fi
    sleep 0.1
  done
}

release_lock() {
  rm -rf "$(meebis_dir)/.lock"
}
