#!/usr/bin/env bash
# Claude Code statusLine helper for the marshal-shim plugin.
#
# Renders a PS1-style prefix (`[user@host dir]`) followed by the
# marshal nickname for THIS Claude Code session, e.g.
#
#     [trevor@laptop marshal] marshal-3 main
#
# Mechanism: marshal-shim writes
#   $XDG_STATE_HOME/marshal/shim-by-ppid-<ppid>.json
# (defaulting to ~/.local/state/marshal/...) on startup and on every
# liveness tick. The shim and this script are both subprocesses of
# the same Claude Code process, so they share a PPID — we just look
# up the file keyed by *our own* PPID.
#
# Portable across macOS + Linux; no /proc, no jq, no python.

set -u

# Read Claude Code's JSON payload so we can pull cwd / model out.
input=$(cat 2>/dev/null || true)

# --- PS1-style prefix (matches the user's bash PS1) -------------------------

cwd=""
if [[ -n "$input" ]]; then
    # Extract .workspace.current_dir or .cwd without jq.
    cwd=$(printf '%s' "$input" \
        | sed -n 's/.*"current_dir"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
        | head -n1)
    if [[ -z "$cwd" ]]; then
        cwd=$(printf '%s' "$input" \
            | sed -n 's/.*"cwd"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
            | head -n1)
    fi
fi
[[ -z "$cwd" ]] && cwd=$(pwd)

user=$(id -un 2>/dev/null || whoami)
# `hostname -s` works on Linux; macOS supports it too.
host=$(hostname -s 2>/dev/null || hostname)
dir=$(basename "$cwd")

prefix="[$user@$host $dir]"

# --- marshal nickname (from the shim's PPID-keyed state file) ---------------

state_dir="${XDG_STATE_HOME:-$HOME/.local/state}/marshal"
state_file="$state_dir/shim-by-ppid-$PPID.json"

nickname=""
if [[ -r "$state_file" ]]; then
    nickname=$(sed -n 's/.*"nickname"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
        "$state_file" | head -n1)
fi

if [[ -n "$nickname" ]]; then
    printf '%s %s\n' "$prefix" "$nickname"
else
    printf '%s\n' "$prefix"
fi
