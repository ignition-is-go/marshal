#!/usr/bin/env bash
# Claude Code SessionEnd hook for marshal.
#
# Removes this session's roster entry on a clean exit so it disappears
# immediately rather than waiting for the staleness sweeper. The sweeper
# remains the fallback for crashes where this hook never fires.
#
# stdin: Claude Code SessionEnd JSON ({session_id, ...}).

source /usr/local/lib/marshal/mcp.sh

IN=$(cat)
SID=$(printf '%s' "$IN" | jq -r '.session_id // empty' 2>/dev/null)
[ -z "$SID" ] && exit 0

marshal_mcp "tools/call" \
  "$(jq -nc --arg sid "$SID" '{name:"deregister",arguments:{session_id:$sid}}')" >/dev/null 2>&1
exit 0
