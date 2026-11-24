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
tagged with `execution` and containing a published `AttachmentPlan` in `plan`.
`client_local` plans run in the consuming account. Pulse uses this mode to retain
that account's authenticated Pulse configuration. `owner_local` plans run in the
launch daemon's account. Native tmux and SSH use this mode. Native tmux
observations capture the tmux command arguments at their source. SSH observations
reuse the owner's configured `MARSHAL_LAUNCH_SSH` executable and options,
then quote each argument of the remote tmux command.
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

## Reach the execution owner

Native plans require an explicit route. Without one, resolution returns
`unsupported`, even when the service URL is loopback.

From a PC, select the daemon's SSH account:

```sh
marshal-launch attach <launch-id> --daemon http://owner:6156 --on user@owner
```

The local SSH client uses your SSH configuration. It runs the installed
`marshal-launch attach` on the owner host with `--owner-local`. That command
checks the backend status again before executing its owner-local plan. The
owner's SSH backend keeps its own SSH configuration for the worker destination.

`--owner-daemon <url>` overrides the API authority used by the remote command.
The default is the exact selected `--daemon` authority. For a local port forward,
supply the owner's reachable API URL explicitly. Failure to reach that URL fails
attachment without another service lookup or replacement launch.

`--owner-program </absolute/path>` selects a trusted installed remote executable.
Its default is `marshal-launch` on the owner's PATH. `--owner-local` explicitly
asserts that the current CLI account is the execution owner. It is exclusive with
`--on`. The same options apply to `attachment-plan`, whose output preserves them.
Marshal daemon credential variables are removed from native clients. Client-local
provider plans retain the consuming account's environment.
