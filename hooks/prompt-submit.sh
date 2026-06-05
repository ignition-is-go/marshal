#!/usr/bin/env bash
# Claude Code UserPromptSubmit hook for marshal.
#
# Fires on every turn the operator submits. Fetches unread messages
# addressed to this session, surfaces them into context, and acks them.
# This is the receive path — replaces channel push with a pull at a
# defined, operator-initiated boundary.
#
# stdin: Claude Code UserPromptSubmit JSON ({session_id, ...}).
# stdout: added to the agent's context (unread peer messages, if any).

source /usr/local/lib/marshal/mcp.sh

IN=$(cat)
SID=$(printf '%s' "$IN" | jq -r '.session_id // empty' 2>/dev/null)
[ -z "$SID" ] && exit 0

marshal_surface_unread "$SID"
exit 0
