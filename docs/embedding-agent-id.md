# Embedding-agent metadata

`MARSHAL_AGENT_ID` attaches an embedding application's agent identifier to a Marshal session. Set it in the environment of the harness and its Marshal processes. Surrounding whitespace is removed. Missing or blank values leave the field unspecified.

Marshal treats the value as self-reported correlation metadata. It does not authenticate an agent, change the operator, select a session, or provide a message address. Continue to address messages by Marshal session ID, nickname, or operator. Multiple sessions may report the same embedding identifier.

The Session wire field is optional `agentId`. Marshal MCP roster and whoami resources expose `agent_id`. Pi, Prime Agent, and OpenCode include it in their native tool output as a JSON-escaped string. Older Session payloads remain valid without the field.

Codex lifecycle hooks, eager app-server bridge registration, and Claude wait requests send the value in a JSON body. An omitted or blank value preserves existing metadata; a nonempty value replaces it. Session end removes the row. A later registration reads the environment again.

The ordinary Claude shim uses `RegisterSession` to preserve hook-supplied metadata when the shim's environment omits it. Hook and shim registration share a server-side lock across their read and write. The shim uses the existing `DeleteSession` command so deletion follows registration on the connection's FIFO command queue. Native plugins retain their cached Session payload across reconnects.

A daemon that does not implement `RegisterSession` receives the legacy Session SET only after its explicit unknown-command response. That fallback preserves connectivity but cannot guarantee metadata preservation. Timeouts and other command errors never trigger fallback. Older clients that send full Session SETs can still remove the field. Use an updated daemon and clients when relying on this metadata.

This capability does not change Pulse Agents bindings or establish that any external control plane consumes the field.
