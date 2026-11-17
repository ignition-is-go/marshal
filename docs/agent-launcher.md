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
        │  marshal-shim launch ssh claude --host build-03 --cwd ~/work/app --task "..."
        ▼
 ┌─────────────── launcher (marshal-shim) ───────────────┐
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
- **The daemon address** the shim already reads (`marshal/daemon-address`, then
  `MARSHAL_DAEMON_ADDRESS`). The launcher passes it to the new agent.
- **`SendMessage`**, for the first task. Direct mail wakes an idle session through its hooks.
- **The trust model**, unchanged: agent identities are self-asserted, and coordination stays on
  loopback or the private mesh ([browser-auth.md](browser-auth.md)). The launcher adds no credentials
  and no new listener. A remote agent can join only if the machine it lands on can reach the daemon
  over the mesh. Making sure of that is the adapter's (or the machine's) job, not Marshal's.

## Commands

```
marshal-shim launch <adapter> <harness> [--cwd <dir>] [--name <label>] [--task <text>]
                    [--timeout <secs>] [-- <adapter options>]
marshal-shim launch list                    # what was launched, its adapter, and roster state
marshal-shim launch stop <name|agent_id>    # ask the adapter to stop it; the row clears on exit
marshal-shim tell <session|nickname> <text> # send a direct message from a terminal
```

- `<harness>` is `claude` or `codex` to start (Codex through `codex-run`, for live wakes). Other
  harnesses (OpenCode, Pi, Prime Agent) come later, as each gains a way to start it from a command line.
- `launch` exits 0 and prints the session id and nickname once the agent joins. If the adapter fails or
  the timeout passes, it exits nonzero and says which. It also says what may still be running, and how
  to stop it.
- `tell` is new and is used by `launch --task`. It is useful on its own for scripts and operators, who
  have no MCP client.

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
  "env": { "MARSHAL_DAEMON_ADDRESS": "ws://marshal.mesh:6155", "MARSHAL_AGENT_ID": "launch-6f1c…",
           "MARSHAL_OPERATOR": "…" },
  "options": ["--host", "build-03"]
}
```

Response: `{"version": 1, "handle": "<opaque, the adapter's own>", "where": "build-03 tmux:claude-app",
"attach": "ssh -t build-03 tmux attach -t =claude-app"}`. Exit nonzero with a message on stderr if
nothing was started.

**`stop`** — request `{"version": 1, "handle": "…"}`. **`status`** (optional) — request
`{"version": 1, "handle": "…"}`, response `{"running": true|false}`. It lets `launch list` tell "exited
before joining" from "still starting".

Rules every adapter keeps:

1. **Run `argv` as given, with `env` added, in `cwd`.** Add no flags to the harness. In particular,
   add no permission-bypass flags: an agent launched this way gets the harness's default permission
   mode. Neither the launcher nor an adapter can be used to start an agent with more permission than
   one started by hand on that machine.
2. **Pass no secrets through `env`.** None of the launcher's variables are secrets. Harness
   credentials (a Claude or Codex login, API keys) belong to the target machine and are set up there,
   the way that machine's owner chooses.
3. **Start nothing if the target can't be reached,** and say so on stderr.

The launcher keeps a small record of each launch: adapter, handle, agent id, name and time, in
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
2. **Who may launch.** Any agent that can run `marshal-shim` could launch more agents. Should launching
   be limited (operator only, a cap on concurrent launches, an allow-list of adapters)? If so, where:
   the launcher, or a daemon-side record?
3. **Cleanup and cost.** VMs that outlive their agent cost money. Should the launcher stop the target
   when the agent's roster row has been gone for a while, or is that left to each adapter?
4. **Pulse.** Is `pulse_agents_execution` something Marshal should drive through an adapter, or the
   other way round?
5. **Recording launches in the daemon** as an entity (visible in the TUI and UI), rather than only in a
   local file. That's more useful for a fleet, but it adds daemon surface. Start local and move later?

## Pull requests

1. This document, for discussion.
2. `marshal-shim tell`, then `launch` with the built-in `tmux` adapter, `launch list | stop`, and e2e
   tests against an in-process daemon: launch, join by `agent_id`, first task delivered, stop clears the
   row, an unreachable daemon starts nothing.
3. The adapter contract (`marshal-launch-<name>` discovery, request/response v1), with a fake adapter in
   tests and `marshal-launch-ssh` as the reference.
4. Further adapters, as the open questions settle.
