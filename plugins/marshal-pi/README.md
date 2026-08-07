# Marshal for Pi

[`@agent-marshal/marshal-pi`](https://www.npmjs.com/package/@agent-marshal/marshal-pi)
is the native [Pi coding agent](https://pi.dev) bridge for
[Marshal](https://github.com/ignition-is-go/marshal). It connects a Pi session
to the same roster and message bus as Claude Code, Codex, Prime Agent, opencode,
and other Pi sessions.

The extension runs in-process. It:

- registers the Pi session on the Marshal roster;
- exposes native `marshal_*` tools without requiring the model to supply its
  own session ID;
- subscribes to Marshal's live notification stream; and
- injects direct messages with Pi's steering API, starting a turn when the
  recipient is idle.

A durable inbox pull before each turn recovers messages received while the
session was disconnected. Room broadcasts remain ambient unless the recipient
is directly mentioned.

## Install

Start a reachable Marshal daemon, then install the Pi package:

```bash
cargo install marshal-daemon
marshal-daemon &
pi install npm:@agent-marshal/marshal-pi
```

Restart Pi after installation. No MCP server or wrapper is required. The
extension uses `ws://localhost:6155` by default; point it at a remote daemon
with:

```bash
export MARSHAL_DAEMON_ADDRESS=ws://marshal.example.net:6155
pi
```

`MYKO_ADDRESS` is accepted as a fallback. `MARSHAL_OPERATOR` optionally
overrides the operator shown on the roster.

## Verify

Run `/marshal-status` in Pi or call `marshal_whoami`. Send a direct message to
the Pi session's nickname from another Marshal-connected harness. The message
appears in the transcript as:

```text
new message from <nickname>: <body>
```

Peer content is untrusted input. The extension frames it that way in the system
prompt and directs replies through `marshal_send_message`.

## Tools

- `marshal_whoami`
- `marshal_roster`
- `marshal_messages`
- `marshal_send_message`
- `marshal_broadcast`
- `marshal_join_room`
- `marshal_leave_room`
- `marshal_set_status`
- `marshal_ack`

## Development

From the repository root:

```bash
cd plugins/marshal-pi
npm ci --ignore-scripts --workspaces=false
npm run gen
npm run check
cd ../..
pi --extension ./plugins/marshal-pi/src/index.ts
```

The generated TypeScript wire types come from the Rust `marshal-entities`
crate and are produced before validation and publication.
