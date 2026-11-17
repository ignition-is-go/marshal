# Agent launcher: start an agent anywhere, and see it join

**Status:** implemented as `crates/launch` (`marshal-launch`, with the built-in `tmux` adapter) and
`crates/launch-ssh` (`marshal-launch-ssh`, the reference external adapter). The open questions at the
end are still open.

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

## What it builds on (already in Marshal)

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
  never appears and the launch only times out. A daemon whose hook listener isn't on port 6156 of the launch address's
  host (test ports, say) is named by `MARSHAL_BASE_URL`. The launcher passes that to every adapter
  with the launch address, and refuses a loopback one for any adapter but `tmux`, as it does the launch
  address.
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
               [--daemon <url>] [--timeout <secs>] [--no-check] [-- <adapter options>]
marshal-launch list [--daemon <url>]   # what was launched, its adapter, and roster state
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
  "attach": "ssh -t build-03 tmux attach -t '=claude-app'"
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

1. **Run `argv` as given, with `env` added, in `cwd`.** An adapter's own options may add variables that
   belong to the target (the ssh adapter's `--oauth-token-file` sets `CLAUDE_CODE_OAUTH_TOKEN` from a
   file there). Add no flags to the harness. In particular,
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
   one version, and Marshal has no other rule for what a daemon accepts), that the harness's hooks and MCP server point at `marshal-shim`,
   and that the target has no `marshal/daemon-address` file naming a different address. The shim reads
   that file ahead of `MARSHAL_DAEMON_ADDRESS`, so such a file would silently send the agent to another
   daemon. (Marshal's own comments disagree: the `ADDRESS_FILE` doc comment
   and `channels.rs` say the environment variable wins. The launcher follows what the shim's connect
   path actually does, `first_daemon_address(file, env, legacy)`. Settling that order is a separate fix.) The adapter runs
   these checks over its own channel (ssh, the VM agent, the container exec). Otherwise each of these
   failures shows up only as a timeout with no reason. `launch` runs `check` first unless `--no-check`
   is given. Marshal has no `doctor` subcommand. If one is added later, `check` can call it instead.

### What a target must already have

The launcher carries none of these. A machine image, a provisioning script or the adapter itself puts
them there:

- `marshal-shim` at a version compatible with the daemon, with the harness's MCP server and hooks wired
  to it;
- the harness (`claude` or `codex`), and `tmux` if the adapter uses it;
- **the harness login.** This is the one real secret: an API key or a setup token per machine. Rule 2
  keeps it out of the launch, so it lives on the target, put there by whoever provisions the machine.
  A `claude` started over ssh on macOS can't reach the login Keychain. So the ssh adapter's
  `--oauth-token-file <path>` names a file on the target holding a `claude setup-token` token, which the
  agent's shell reads as it starts. Only the path travels;
- the working folder, already trusted by the harness (open question 1);
- a network path to the launch address (the mesh).

**macOS targets.** macOS Local Network privacy can silently stop a launched agent from joining.
This was seen on a macOS target with `claude` running in a tmux server that `marshal-launch-ssh` had
started over ssh. Both of the agent's `marshal-shim` processes, its MCP server and its `SessionStart`
hook, had their connections to a daemon on the local network dropped. The target's kernel log showed
`tcp drop outgoing … reason: NECP`, and no packet left the machine. The shim logs only that it
disconnected. Other programs in a tmux session started the same way (`bash`'s `/dev/tcp`, `nc`)
reached the daemon. Nothing was listed under System Settings → Privacy & Security → Local Network. A
`claude` started from the desktop's Terminal reached the same daemon. What works over ssh: start a
tmux server from the desktop session first (in Terminal, `tmux new-session -s desktop`, left
running). The sessions `marshal-launch-ssh` creates then go into that server, on tmux's default
socket, and a launched agent joined within seconds. If no server is running when the launch comes,
the adapter's `tmux new-session` starts one over ssh, and the drop above follows.

### Join proof

`agent_id` is self-asserted, so any session could present `launch-<uuid>`. The launcher therefore
also checks that the row's `host.name` and `cwd` match the `host` and `cwd` the adapter reported in its
`launch` response. That isn't authentication, which Marshal's trust model leaves to
the mesh. It is enough that a stray or mistaken session isn't taken for the launched one.

### First task, with nobody at the keyboard

Rule 1 adds no flags, so a launched Claude has no channels flag. Mail then reaches it through its
hooks: the `claude-hook wait` waiter wakes an idle session. The e2e test for "first task delivered"
uses a Claude with nobody typing, so a launched agent can't sit idle with its task unread.

`marshal-launch` keeps a small record of each launch: adapter, handle, agent id, name, time and the
daemon it was handed, in `$XDG_STATE_HOME/marshal/launches.json`. `marshal-launch list` and
`marshal-launch stop` use it. `list` reads each launch's roster state from its recorded daemon, and
`list --daemon <url>` overrides that.

## Adapters

| Adapter | Where the agent runs | Status |
|---|---|---|
| `tmux` (built in) | a detached tmux session on this machine | built in, the default |
| `ssh` (`marshal-launch-ssh`) | a tmux session on a mesh host, over ssh | the reference external adapter |
| VM / cloud, container, others | a new VM or container that boots the harness | later, out of tree, through the same contract |

## Open questions

1. **Headless first run.** A remote agent has no person at its terminal. Claude Code asks once per new
   folder whether to trust it, and that prompt is a safety feature. The launcher expects the target
   machine's owner to pre-trust the working folder there, using the harness's own settings, and it never
   bypasses the prompt. Is that acceptable, or does the fleet need something else?
2. **Who may launch.** Any agent that can run `marshal-launch` could launch more agents. Should launching
   be limited (operator only, a cap on concurrent launches, an allow-list of adapters)? If so, where:
   the launcher, or a daemon-side record?
3. **Cleanup and cost.** VMs that outlive their agent cost money. Should the launcher stop the target
   when the agent's roster row has been gone for a while, or is that left to each adapter?
