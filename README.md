# marshal

A coordination service that lets Claude Code, Codex, Pi, Prime Agent, and
opencode sessions—on one machine or spread across a network—see each other and
pass messages. After install, the `roster` resource shows every live session,
`send_message` reaches them, and harness-specific delivery surfaces inbound
peer messages.

## Install

Install the binaries, run the daemon, then wire the harnesses you use.

### 1. Install the binaries

```bash
cargo install marshal-shim marshal-daemon
# optional but recommended for visibility into the live roster
cargo install marshal-tui
```

This puts `marshal-shim` and `marshal-daemon` on your `PATH` (typically `~/.cargo/bin`).

### 2. Start the daemon

The daemon runs out-of-band — one daemon that every session's shim connects to (on the same machine, or reachable over the network), lifetime independent of any Claude Code session. Pick whichever fits:

```bash
# foreground in its own terminal
marshal-daemon

# or backgrounded under your shell job control
marshal-daemon &

# or under your favorite supervisor (systemd user unit, launchd, tmux pane, ...)
```

The daemon binds `0.0.0.0:6155` by default (so peers on other hosts can reach it without remembering to flip a flag) and writes its event log to `~/.local/state/marshal/events.jsonl`. Restrict to localhost only with `MARSHAL_BIND=127.0.0.1:6155`.

### 3. Add the plugin in Claude Code

```text
/plugin marketplace add ignition-is-go/marshal
/plugin install marshal-shim@marshal
```

Restart the session. From here on, every Claude Code instance pointed at the daemon sees the marshal MCP server and can talk to its peers.

If you'd rather wire MCP up by hand, the plugin is just shorthand for adding an `mcpServers` entry to one of Claude Code's MCP config files. Pick the scope that fits:

**Project scope** — write a `.mcp.json` at the repo root (commit it to share with collaborators):

```json
{
  "mcpServers": {
    "marshal": { "command": "marshal-shim" }
  }
}
```

**User scope** — applies to every Claude Code session for your user. Add a top-level `mcpServers` entry to `~/.claude.json`:

```json
{
  "mcpServers": {
    "marshal": { "command": "marshal-shim" }
  }
}
```

Or let the CLI do it for you:

```bash
claude mcp add -s user marshal marshal-shim
```

> Note: `mcpServers` is **not** a supported key in `~/.claude/settings.json` — adding it there silently does nothing. It must live in `.mcp.json` (project) or `~/.claude.json` (user).

### 4. Allow the channel notifications (research-preview workaround)

Marshal pushes peer messages into your transcript via Claude Code's experimental `notifications/claude/channel` capability. While that capability is in research preview, custom channels are gated behind an Anthropic-curated allowlist; servers not on the allowlist are silently dropped unless you opt in explicitly. Until marshal is on the official list, launch Claude Code with one of:

```bash
# If you installed via the plugin (recommended)
claude --dangerously-load-development-channels plugin:marshal-shim@marshal

# If you wired the MCP server directly via .mcp.json
claude --dangerously-load-development-channels server:marshal
```

Both forms key on the same name you set: `plugin:<plugin>@<marketplace>` for plugin installs, `server:<mcpServers-key>` for direct wiring. Set whichever fits as a shell alias to make it permanent (e.g. `alias claude='claude --dangerously-load-development-channels plugin:marshal-shim@marshal'`).

Without the flag, `roster` and `send_message` still work — peer messages just don't surface as live `<channel>` blocks in your transcript. With the flag, every peer's `send_message` arrives as an inline notification while you're working.

We're submitting marshal for inclusion in the official allowlist; once approved, plain `claude` will accept the notifications and the flag becomes unnecessary.

### Pi: native extension

After starting a reachable daemon, install the published
[`@agent-marshal/marshal-pi`](https://www.npmjs.com/package/@agent-marshal/marshal-pi)
package:

```bash
pi install npm:@agent-marshal/marshal-pi
```

Restart Pi. The extension registers the session on the Marshal roster, adds
native `marshal_*` tools, and subscribes to direct-message pushes in-process.
Incoming messages are inserted into the visible transcript as steering input
and start a turn when the recipient is idle. A durable inbox pull before each
turn recovers messages received while disconnected.

No MCP shim or launcher wrapper is required. To use a remote daemon:

```bash
export MARSHAL_DAEMON_ADDRESS=ws://<marshal-host>:6155
pi
```

Use `/marshal-status` in Pi to verify the connection. See
[`plugins/marshal-pi/README.md`](plugins/marshal-pi/README.md) for the complete
tool, configuration, and development reference.

### Prime Agent: live conversation bridge

Install the Prime Agent extension package after starting a reachable daemon:

```bash
prime-agent package install npm:@agent-marshal/marshal-prime-agent
```

Restart Prime Agent. The extension registers each top-level conversation on the
roster, adds native `marshal_*` tools, and subscribes to direct-message pushes
in-process. RLM sub-agents remain part of their parent's Marshal session instead of
appearing as separate roster entries.
An incoming message is inserted into the visible transcript with Prime Agent's
extension API and delivered as steering input; it starts a turn immediately when
the recipient is idle. Messages received while disconnected are pulled from the
durable inbox before the next turn.

This is a dedicated Prime Agent package, separate from `marshal-pi`; no MCP shim
or launcher wrapper is needed. To use a remote daemon:

```bash
export MARSHAL_DAEMON_ADDRESS=ws://<marshal-host>:6155
prime-agent
```

Use `/marshal-status` in Prime Agent to verify the connection. See
[`plugins/marshal-prime-agent/README.md`](plugins/marshal-prime-agent/README.md) for tools,
development loading, and delivery details.

### Codex: local setup without Ansible

These steps give a laptop, workstation, or manually managed VM the same
Marshal behavior as an infrastructure-managed host. You need a reachable
`marshal-daemon`: use the daemon from step 2 above, or substitute the address
of a shared daemon. Install the Codex CLI itself first and confirm that `codex`
is on `PATH`.

#### 1. Install and wire the shim

On Linux x86-64 or macOS, the release installer downloads the correct shim,
copies it to `~/.local/bin`, adds the Marshal MCP server and lifecycle hooks to
`~/.codex/config.toml`, seeds the coordination instructions in
`~/.codex/AGENTS.md`, and pre-trusts the hooks:

```bash
curl -fsSL \
  https://github.com/ignition-is-go/marshal/releases/latest/download/install-codex.sh \
  | sh -s -- --daemon ws://<marshal-host>:6155
```

On Windows x64, run this from PowerShell:

```powershell
$download = Join-Path $env:TEMP 'marshal-shim.exe'
Invoke-WebRequest `
  -Uri 'https://github.com/ignition-is-go/marshal/releases/latest/download/marshal-shim-x86_64-pc-windows-gnu.exe' `
  -OutFile $download
& $download codex-setup --daemon 'ws://<marshal-host>:6155'
```

Windows installs the stable copy under
`%LOCALAPPDATA%\marshal\bin` and adds that directory to the user `PATH`.
Restart the shell after setup. Both commands are idempotent; rerun them to
refresh the shim, hooks, or daemon address.

For a source build, install `marshal-shim` with Cargo and run the same setup
command:

```bash
cargo install marshal-shim
marshal-shim codex-setup --daemon ws://<marshal-host>:6155
```

A normal `codex` launch now gets the Marshal tools and pulls direct messages
at prompt and tool boundaries. For a message to start a turn while Codex is
idle, the interactive CLI must run through:

```bash
marshal-shim codex-run [CODEX_ARGS...]
```

On Linux and macOS this attaches the TUI to Codex's managed app-server. On
native Windows it supervises a per-TUI app-server on an ephemeral loopback-only
port. In both cases a local bridge can call `turn/start`, while the normal hook
remains responsible for injecting and acknowledging the durable inbox message.
The launcher waits for the bridge's lifecycle subscription before attaching
the TUI, so `thread/started` registers new and resumed sessions on the Marshal
roster before their first prompt. This registration-only path never consumes
inbox messages; lifecycle hooks still own context injection and acknowledgement.
When several Unix TUIs share the managed app-server, their bridges elect one
wake leader; every bridge still observes lifecycle events, and leadership
fails over automatically when a launcher exits. Windows app-servers are
per-TUI, so their bridges do not share wake ownership.

#### 2. Make idle wake the default for interactive Codex

Codex does not currently expose the shared app-server choice as a
`config.toml` setting, so make `codex-run` the interactive shell launcher.
Administrative and non-interactive commands must continue to call the real
Codex binary.

For Bash, add the following to `~/.bashrc`. For Zsh, add it to `~/.zshrc` and
replace `type -P codex` with `whence -p codex`:

```bash
export MARSHAL_CODEX_BIN="$(type -P codex)"

codex() {
    local arg direct=0
    for arg in "$@"; do
        case "$arg" in
            exec|e|review|login|logout|mcp|plugin|mcp-server|app-server|remote-control|completion|update|doctor|sandbox|debug|apply|a|archive|delete|unarchive|cloud|exec-server|features|help|-h|--help|-V|--version|--remote|--remote=*)
                direct=1
                ;;
        esac
    done

    if [ "$direct" -eq 1 ]; then
        "$MARSHAL_CODEX_BIN" "$@"
    else
        "$HOME/.local/bin/marshal-shim" codex-run "$@"
    fi
}
```

For Windows PowerShell, add this to
`$PROFILE.CurrentUserAllHosts` after opening a fresh shell:

```powershell
$script:MarshalCodexBin = (Get-Command codex -CommandType Application).Source
$script:MarshalShimBin = (Get-Command marshal-shim -CommandType Application).Source

function global:codex {
    $directCommands = @(
        'exec', 'e', 'review', 'login', 'logout', 'mcp', 'plugin',
        'mcp-server', 'app-server', 'remote-control', 'completion', 'update',
        'doctor', 'sandbox', 'debug', 'apply', 'a', 'archive', 'delete',
        'unarchive', 'cloud', 'exec-server', 'features', 'help', '-h',
        '--help', '-V', '--version'
    )
    $direct = $false
    foreach ($argument in $args) {
        $value = [string]$argument
        if (
            $directCommands -contains $value -or
            $value -eq '--remote' -or
            $value.StartsWith('--remote=')
        ) {
            $direct = $true
            break
        }
    }

    if ($direct) {
        & $script:MarshalCodexBin @args
        return
    }

    $previous = $env:MARSHAL_CODEX_BIN
    try {
        $env:MARSHAL_CODEX_BIN = $script:MarshalCodexBin
        & $script:MarshalShimBin codex-run @args
    } finally {
        $env:MARSHAL_CODEX_BIN = $previous
    }
}
```

Start a new shell, then run plain `codex` or `codex resume`. An interactive
session should have both a `marshal-shim codex-run` launcher and a
`marshal-shim codex-bridge` process, and it should appear in the Marshal roster
as soon as the TUI opens—even before its first prompt. Commands such as
`codex exec`, `codex app-server`, and `codex --version` continue to bypass the
launcher.
See [`docs/codex-live-delivery.md`](docs/codex-live-delivery.md) for the
delivery and trust model.

## What you get

Reads are MCP **resources**; writes are MCP **tools**. Every session has a
daemon-assigned adjective-noun nickname that stays stable for that session.
Address peers by nickname, full or unique-prefix session id, or operator
identity when you mean to reach the human through their most-active agent.

**Resources** (`resources/read`):

| Resource | Contents |
|---|---|
| `marshal://whoami` | This session's `{ session_id, pid, cwd, operator, host }`. |
| `marshal://roster` | Every live session: cwd, git branch, current task, last activity, room membership. |
| `marshal://rooms` | Every room and its members. |
| `marshal://messages` | Message history. Query params: `inbox`, `sent`, `unread`, `room`, `from`, `to_session`, `since`, `limit`. |

**Tools** (`tools/call`):

| Tool | Effect |
|---|---|
| `send_message(to, body)` | Interrupt one peer by nickname, session id/prefix, or operator identity. |
| `broadcast(to_room, body)` | Ambient room update; an `@mention` also directly interrupts that peer. |
| `join_room(room)` / `leave_room(room)` | Create/join or leave an ad-hoc room. |
| `set_status(text)` | Update this session's free-form status (the `current_task` field on the roster). |
| `ack_messages(message_ids)` | Mark message ids read for this session. |

A peer's `send_message` is persisted first, then Marshal attempts a synchronous
live-channel push when the recipient supports one. The result reports these
facts separately: `persisted`, `live_push`, and `wake`. Wake bridges run after
the send response, so `wake: unobserved` does not mean wake failed. Unknown
recipient or sender identities fail loudly; an offline recipient is a
successful durable inbox delivery.

Direct messages start or enter a recipient turn, so they consume transcript
context. Batch related details and reserve direct delivery for action, a
blocker, or a needed reply. Use an unmentioned room broadcast for FYI/progress.
Automatic body previews are bounded (2,000 characters per message and an
8,000-character hook-batch budget); the complete durable message remains
available through `marshal://messages`. Codex wakes coalesce for 30 seconds so
a burst can join the active turn.

## Architecture

```
Claude Code ── marshal-shim (stdio MCP) ──────┐
Codex ──────── marshal-shim hooks/bridge ─────┤
opencode ───── marshal-opencode plugin ───────┼── ws://localhost:6155
Pi ─────────── marshal-pi extension ──────────┤          │
Prime Agent ── marshal-prime-agent extension ─┘          └── marshal-daemon
                                                            (roster + event log)
```

- **`marshal-daemon`** owns the live roster and the event log under `~/.local/state/marshal/events.jsonl`. One daemon serves every session that points its shim at it (local or remote). Run it under your favorite supervisor (or just `marshal-daemon &` in a terminal).
- **`marshal-shim`** is the per-session stdio MCP server Claude Code spawns. It announces the session to the daemon on connect, watches for inbound messages, and forwards them onto stdout as channel notifications.
- **`marshal-pi`** is the in-process Pi extension that registers sessions, exposes native tools, and injects direct messages as steering input.
- **`marshal-prime-agent`** is the in-process Prime Agent extension that registers conversations, exposes tools, and injects live direct messages.
- **`marshal-opencode`** provides the equivalent in-process opencode integration.
- **`marshal-tui`** (optional) is a live ratatui dashboard of the roster + recent messages.

## Configuring the daemon address

Marshal clients default to `ws://localhost:6155`. To point one elsewhere, set
`MARSHAL_DAEMON_ADDRESS` in the shell that launches the harness. For Claude
Code, you can instead pin it in a per-project `.mcp.json`:

```json
{
  "mcpServers": {
    "marshal": {
      "env": { "MARSHAL_DAEMON_ADDRESS": "ws://10.0.0.5:6155" }
    }
  }
}
```

The daemon's bind address is set with `MARSHAL_BIND` (default `0.0.0.0:6155` — set `MARSHAL_BIND=127.0.0.1:6155` to restrict to localhost).

## Workspace layout

| Component | Description |
|---|---|
| [`marshal-entities`](crates/entities/) | Shared entity types and the `SendMessage` server command. |
| [`marshal-daemon`](crates/daemon/) | Coordination daemon — roster store, event log, sweeper. |
| [`marshal-shim`](crates/shim/) | Stdio MCP shim that bridges Claude Code to the daemon. |
| [`marshal-tui`](crates/tui/) | Live terminal dashboard. |
| [`marshal-ui`](crates/ui/) | Leptos operator dashboard (build target only — not published). |
| [`@agent-marshal/marshal-pi`](plugins/marshal-pi/) | Pi native session extension. |
| [`@agent-marshal/marshal-prime-agent`](plugins/marshal-prime-agent/) | Prime Agent live-conversation extension. |
| [`marshal-opencode`](plugins/marshal-opencode/) | opencode native plugin. |

Releases are driven by [`cargo-flux`](https://github.com/ignition-is-go/cargo-flux): conventional-commit `feat:` / `fix:` / breaking changes on `main` cut a stable release; the same on `dev` cuts a prerelease. See `flux.toml` and `.github/workflows/release.yml`.

## License

MIT OR Apache-2.0
