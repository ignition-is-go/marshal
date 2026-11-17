# Agent launcher: start an agent anywhere, and see it join

**Status: design proposal, for discussion.** Nothing here is implemented upstream yet. A working
prototype (local tmux only) exists in the Castor-o7 fork as `marshal-shim spawn`.

## The ask

> "We want the ability to have an adapter pattern that allows anything that can spin up an agent to do
> so for the purposes of getting an agent active and working on VMs and others that are not on the same
> machine."

## The idea in one paragraph

Marshal already knows who every agent is and what it's doing: that's the roster. What it can't do is
*start* one. Starting an agent has one part that depends on where it runs (a tmux pane here, a shell
over ssh, a cloud VM's boot script, a container, a hosted agent service) and several parts that don't:
choose an id, tell the agent where the daemon is, wait for it to appear on the roster, and hand it its
first task. The launcher owns the parts that don't vary. An **adapter** does only the part that does:
*run this command, with these environment variables, over there*. An adapter is never asked whether
the launch worked. The roster answers that: the agent showing up is the proof.

```
 operator or agent
        │  marshal-launch ssh claude --host build-03 --cwd ~/work/app --task "..."
        ▼
 ┌────────────── launcher (marshal-launch) ──────────────┐
 │ 1. agent_id = launch-<uuid>                            │
 │ 2. env = MARSHAL_DAEMON_ADDRESS, MARSHAL_AGENT_ID, ... │
 │ 3. hand {harness, cwd, env} to the adapter ────────────┼──► marshal-launch-ssh ──► build-03:
 │ 4. watch the roster for agent_id                ◄──────┼──── harness starts, its hooks register
 │ 5. send the first task to that session                 │     the session with agent_id
 └────────────────────────────────────────────────────────┘
```

## What it builds on (already upstream)

- **`MARSHAL_AGENT_ID`** ([embedding-agent-id.md](embedding-agent-id.md)) puts a caller-chosen id on
  the session's roster row. The launcher picks a fresh `launch-<uuid>` per launch and waits for the row
  carrying it. As that doc says, it's correlation metadata, not a credential, and the launcher treats
  it the same way.
- **The daemon address.** On the launching machine this is usually `ws://localhost:6155`, which is
  wrong anywhere else. So the launcher passes a separate **launch address**: `--daemon <url>`, or
  `marshal/launch-address` beside `marshal/daemon-address`. It refuses a loopback address for any
  adapter but `tmux`. That address must reach both of the daemon's listeners: `MARSHAL_BIND` (6155)
  and `MARSHAL_HOOK_BIND` (6156). The remote agent's hooks register its row through the hook port, and
  Codex derives the hook URL from the daemon address. If the hook port stays on loopback, the agent
  never appears and the launch only times out.
- **`SendMessage`**, for the first task. Direct mail wakes an idle session through its hooks.
- **The trust model**, unchanged: agent identities are self-asserted, and coordination stays on
  loopback or the private mesh ([browser-auth.md](browser-auth.md)). The launcher adds no credentials
  and no new listener. A remote agent can join only if the machine it lands on can reach the daemon
  over the mesh. Making sure of that is the adapter's (or the machine's) job, not Marshal's.

## Where it lives: its own binary, not the shim

The shim is the agent's side of Marshal, and it should stay that way (see the review on #168). It also
shouldn't give an agent a second way to send messages besides its MCP tool. So the launcher is a
separate crate and binary, **`marshal-launch`**, a Myko client like the TUI. It reads the roster and
sends the first task over its own connection. It is labelled as the launcher, so the task never looks
as if it came from the agent's own tools. An agent that may launch others runs `marshal-launch` as it
runs any program. Marshal grants it nothing more. A later TUI view can call the same library.

```
marshal-launch <adapter> <harness> [--cwd <dir>] [--name <label>] [--task <text>]
               [--daemon <url>] [--timeout <secs>] [-- <adapter options>]
marshal-launch list                    # what was launched, its adapter, and roster state
marshal-launch stop <name|agent_id>    # ask the adapter to stop it; the row clears on exit
```

- `<harness>` is `claude` or `codex` to start (Codex through `codex-run`, for live wakes). Other
  harnesses (OpenCode, Pi, Prime Agent) come later, as each gains a way to start it from a command line.
- It exits 0 and prints the session id and nickname once the agent joins. If the adapter fails or the
  timeout passes, it exits nonzero and says which. It also says what may still be running, and how to
  stop it.
- The first task is one direct message to the joined session's id, sent under a short-lived session of
  kind `launcher`, and its body says who launched the agent. This is the only message `marshal-launch`
  sends. It has no general `tell`.

## The adapter contract

An adapter is an executable named `marshal-launch-<name>` on `PATH`, in the same way `git` and `cargo`
find their subcommands. That keeps cloud SDKs and site-specific tooling out of Marshal's own build, and
lets anyone write one in any language. `tmux` ships built in, as the reference and the default.

The launcher runs `marshal-launch-<name> <verb>` with one JSON object on stdin, and reads one JSON
object from stdout. Anything written to stderr is shown to the user.

**`launch`** — request:

```json
{
  "version": 1,
  "harness": "claude",
  "argv": ["claude"],
  "cwd": "~/work/app",
  "name": "claude-app",
  "agent_id": "launch-6f1c…",
  "marshal_version": "0.60.2",
  "env": { "MARSHAL_DAEMON_ADDRESS": "ws://marshal.mesh:6155", "MARSHAL_AGENT_ID": "launch-6f1c…",
           "MARSHAL_OPERATOR": "…" },
  "options": ["--host", "build-03"]
}
```

Response:

```json
{
  "version": 1,
  "handle": "<opaque, the adapter's own>",
  "host": "build-03",
  "cwd": "/home/agent/work/app",
  "where": "build-03 tmux:claude-app",
  "attach": "ssh -t build-03 tmux attach -t =claude-app"
}
```

`host` is the target's host name, as its `gethostname` reports it. `cwd` is the absolute folder the
harness was started in. The launcher matches both against the joined row (see Join proof). `attach` is
optional. Exit nonzero with a message on stderr if nothing was started.

**`stop`** — request `{"version": 1, "handle": "…"}`. **`status`** (optional) — request
`{"version": 1, "handle": "…"}`, response `{"running": true|false}`. It lets `marshal-launch list` tell "exited
before joining" from "still starting". An adapter without `status` exits 2 for it, which the launcher
reads as "unknown", not as an error. Any other nonzero exit is an error.

Rules every adapter keeps:

1. **Run `argv` as given, with `env` added, in `cwd`.** Add no flags to the harness. In particular,
   add no permission-bypass flags: an agent launched this way gets the harness's default permission
   mode. Neither the launcher nor an adapter can be used to start an agent with more permission than
   one started by hand on that machine.
2. **Pass no secrets through `env`.** None of the launcher's variables are secrets. Harness
   credentials (a Claude or Codex login, API keys) belong to the target machine and are set up there,
   the way that machine's owner chooses.
3. **Start nothing if the target can't be reached,** and say so on stderr.
4. **Check before starting.** A `check` verb (same request as `launch`) confirms, on the target, that
   it can reach the launch address on both ports, that `marshal-shim --version` there has the same
   major.minor as the request's `marshal_version` (the launcher's own; Marshal releases every crate at
   one version, and upstream has no other rule for what a daemon accepts), that the harness's hooks and MCP server point at `marshal-shim`,
   and that the target has no `marshal/daemon-address` file naming a different address. The shim reads
   that file ahead of `MARSHAL_DAEMON_ADDRESS`, so such a file would silently send the agent to another
   daemon. (Upstream's own comments disagree: the `ADDRESS_FILE` doc comment
   and `channels.rs` say the environment variable wins. The launcher follows what the shim's connect
   path actually does, `first_daemon_address(file, env, legacy)`. Settling that order is a separate fix.) The adapter runs
   these checks over its own channel (ssh, the VM agent, the container exec). Otherwise each of these
   failures shows up only as a timeout with no reason. `launch` runs `check` first unless `--no-check`
   is given. Upstream has no `doctor` subcommand. If one is added later, `check` can call it instead.

### What a target must already have

The launcher carries none of these. A machine image, a provisioning script or the adapter itself puts
them there:

- `marshal-shim` at a version compatible with the daemon, with the harness's MCP server and hooks wired
  to it;
- the harness (`claude` or `codex`), and `tmux` if the adapter uses it;
- **the harness login.** This is the one real secret: an API key or a setup token per machine. Rule 2
  keeps it out of the launch, so it lives in the target's own credential store, put there by whoever
  provisions the machine;
- the working folder, already trusted by the harness (open question 1);
- a network path to the launch address (the mesh).

### Join proof

`agent_id` is self-asserted, so any session could present `launch-<uuid>`. The launcher therefore
also checks that the row's `host.name` and `cwd` match the `host` and `cwd` the adapter reported in its
`launch` response. That isn't authentication, which upstream's trust model leaves to
the mesh. It is enough that a stray or mistaken session isn't taken for the launched one.

### First task, with nobody at the keyboard

Rule 1 adds no flags, so a launched Claude has no channels flag. Mail then reaches it through its
hooks: the `claude-hook wait` waiter wakes an idle session. The e2e test for "first task delivered"
uses a Claude with nobody typing, so a launched agent can't sit idle with its task unread.

`marshal-launch` keeps a small record of each launch: adapter, handle, agent id, name and time, in
`$XDG_STATE_HOME/marshal/launches.json`. `launch list` and `launch stop` use it.

## Adapters

| Adapter | Where the agent runs | Status |
|---|---|---|
| `tmux` (built in) | a detached tmux session on this machine | first PR; port of the fork's prototype |
| `ssh` | a tmux session on a mesh host, over ssh | second PR, as the reference external adapter |
| VM / cloud (`cloud-init` and others) | a new VM that boots the harness | later, possibly out of tree |
| container | a container image with the shim and harness | later, possibly out of tree |
| Pulse Agents execution | whatever `pulse-deploy` runs today | to discuss: wrapping it may be the most useful adapter for ignition-is-go |

## Open questions

1. **Headless first run.** A remote agent has no person at its terminal. Claude Code asks once per new
   folder whether to trust it, and that prompt is a safety feature. The proposal says the target
   machine's owner pre-trusts the working folder there, using the harness's own settings, and the
   launcher never bypasses the prompt. Is that acceptable, or does the fleet need something else?
2. **Who may launch.** Any agent that can run `marshal-launch` could launch more agents. Should launching
   be limited (operator only, a cap on concurrent launches, an allow-list of adapters)? If so, where:
   the launcher, or a daemon-side record?
3. **Cleanup and cost.** VMs that outlive their agent cost money. Should the launcher stop the target
   when the agent's roster row has been gone for a while, or is that left to each adapter?
4. **Pulse.** Is `pulse_agents_execution` something Marshal should drive through an adapter, or the
   other way round?
5. **Recording launches in the daemon** as an entity (visible in the TUI and UI), rather than only in a
   local file. A per-machine `launches.json` can't show a swarm started by several agents or hosts
   together, and that's what a fleet needs. But it adds daemon surface. Start local and move later?
6. **Starting many at once.** A team file (each member's adapter, harness, folder and first task),
   started by one `launch --team <file>` that reports who joined and who didn't. The fork's prototype
   has one for tmux. Wanted in the first round, or later?

## Pull requests

1. This document, for discussion.
2. The `marshal-launch` crate with the built-in `tmux` adapter, `list | stop`, and e2e
   tests against an in-process daemon: launch, join by `agent_id`, first task delivered, stop clears the
   row, an unreachable daemon starts nothing.
3. The adapter contract (`marshal-launch-<name>` discovery, request/response v1), with a fake adapter in
   tests and `marshal-launch-ssh` as the reference.
4. Further adapters, as the open questions settle.
