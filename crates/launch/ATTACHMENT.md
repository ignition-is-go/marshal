# Attach to an existing launch

`marshal-launch attachment-plan <launch-id> [--daemon <url>]` returns an
`agent-attachment-protocol` version 1 resolution. Its managed target uses the
`marshal` authority, the selected launch service origin, and the exact launch ID.
An attachable plan invokes `marshal-launch attach` with that ID and origin.

`marshal-launch attach <launch-id> [--daemon <url>]` reads the launch record and
checks current backend status through the existing get operation. It executes the
validated `attachment_plan` argv from the recorded backend observation only when
the worker is confirmed running. It inherits the terminal streams. Neither command
starts or recovers a worker, sends a task, or changes a launch record.

Backend protocol version 3 observations may include an optional `attachment_plan`
using the published `agent-attachment-protocol` `AttachmentPlan` type. Native tmux
observations capture the tmux command arguments at their source. SSH observations
construct the local SSH argv and quote each argument of the remote tmux command.
Pulse observations invoke the authenticated Pulse CLI, which checks its action
catalog again before attachment.

The existing `attach` string is display text. Records without a structured plan
return `unsupported`. Unknown worker status returns `uncertain`. Stopped, exited,
absent, and unstarted workers return `unavailable`. Attachment never substitutes a
new worker. The owner CLI and backend executable must be installed on PATH.

The published protocol rejects control characters and semicolons in command
fields. A backend plan with those values is unsupported even when a shell could
quote them. SSH uses its remote shell transport, with quoting constructed from
backend arguments rather than parsed display text.
