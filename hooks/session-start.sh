#!/usr/bin/env bash
# Claude Code SessionStart hook for marshal.
#
# Registers this session's roster entry keyed by the Claude Code
# session_id (so peers, the inbox query, and the statusline all agree on
# one id), then drains any messages waiting from before this session
# started into context.
#
# stdin: Claude Code SessionStart JSON ({session_id, cwd, ...}).
# stdout: added to the agent's context (the drained backlog, if any).

source /usr/local/lib/marshal/mcp.sh

IN=$(cat)
SID=$(printf '%s' "$IN" | jq -r '.session_id // empty' 2>/dev/null)
[ -z "$SID" ] && exit 0

CWD=$(printf '%s' "$IN" | jq -r '.cwd // .workspace.current_dir // empty' 2>/dev/null)
[ -z "$CWD" ] && CWD="$PWD"
DIR=$(basename "$CWD" 2>/dev/null)
NICK="${DIR:-session}@${SID:0:8}"
OP="${USER:-anonymous}"
HOST=$(hostname -s 2>/dev/null)
ARCH=$(uname -m 2>/dev/null)
BRANCH=$(git -C "$CWD" rev-parse --abbrev-ref HEAD 2>/dev/null)

PARAMS=$(jq -nc \
  --arg sid "$SID" --arg nick "$NICK" --arg cwd "$CWD" --arg op "$OP" \
  --arg proj "$DIR" --arg br "$BRANCH" --arg hn "$HOST" --arg arch "$ARCH" \
  '{name:"register",arguments:(
      {session_id:$sid, nickname:$nick, cwd:$cwd, operator:$op, project:$proj,
       host:{name:$hn, os:"linux", arch:$arch}}
      + (if $br=="" then {} else {git_branch:$br} end)
   )}')
marshal_mcp "tools/call" "$PARAMS" >/dev/null 2>&1

marshal_surface_unread "$SID"
exit 0
