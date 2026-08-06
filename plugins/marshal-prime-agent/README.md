# Marshal for Prime Agent

`@agent-marshal/marshal-prime-agent` is the native Prime Agent bridge for
[marshal](https://github.com/ignition-is-go/marshal). It is separate from the
`marshal-pi` package and targets Prime Agent's conversation lifecycle and canonical
session identities.

The extension runs inside each Prime Agent conversation. It:

- registers the conversation on the Marshal roster using Prime Agent's session ID;
- exposes native `marshal_*` tools to the model;
- subscribes to Marshal's live notification stream; and
- injects incoming direct messages with
  `pi.sendMessage(..., { deliverAs: "steer", triggerTurn: true })`.

A direct message therefore appears in the transcript immediately and starts a turn
when the recipient is idle. While the agent is working, it steers the active run at
the next tool boundary. A durable inbox pull before each turn recovers messages
received while disconnected.

## Install

Start a reachable daemon, then install the Prime Agent package:

```bash
cargo install marshal-daemon
marshal-daemon &
prime-agent package install npm:@agent-marshal/marshal-prime-agent
```

Restart Prime Agent after installation. No MCP server or wrapper is required.
For a remote daemon:

```bash
export MARSHAL_DAEMON_ADDRESS=ws://marshal.example.net:6155
prime-agent
```

The default is `ws://localhost:6155`. `MARSHAL_OPERATOR` optionally overrides the
operator name shown on the roster.

For development:

```bash
cargo run -p marshal-entities --no-default-features --features codegen \
  --bin typegen -- plugins/marshal-prime-agent/src/generated
prime-agent --extension ./plugins/marshal-prime-agent/src/index.ts
```

## Verify

Run `/marshal-status` inside Prime Agent or use `marshal_whoami`. Send a direct
message from another Marshal session to its nickname or session ID. It appears as:

```text
new message from <nickname>: <body>
```

Peer content is untrusted input. The extension adds that warning to the system
prompt and directs replies through `marshal_send_message`. Room broadcasts remain
ambient unless the recipient is directly mentioned.

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
