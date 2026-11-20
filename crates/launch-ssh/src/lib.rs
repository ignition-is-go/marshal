//! `marshal-launch-ssh`: the reference external adapter for `marshal-launch`.
//!
//! It starts an agent in a detached tmux session on another machine, over ssh.
//! It speaks adapter contract v2 (see `docs/agent-launcher.md`, "The adapter
//! contract"): `marshal-launch-ssh <verb>` reads one JSON object on stdin and
//! writes one on stdout. Anything on stderr is shown to the user.
//!
//! Target operations run one `bash -l -s` on the target, with a script this module
//! writes on its stdin. Values reach the script single-quoted, never through
//! the ssh command line, so nothing in a request is parsed by a remote shell.
//! The script reports back in `@@<key> <value>` lines, which [`Probe::parse`]
//! reads; anything else on stdout (a chatty login profile) is ignored.
//!
//! Options (after `--` on the `marshal-launch` command line):
//! `--host <ssh destination>`, required.
//!
//! `MARSHAL_LAUNCH_SSH` replaces the `ssh` program (split on whitespace), for
//! a site wrapper or for tests.

use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use marshal_launch::address::{host_port, is_loopback_host};
use marshal_launch::external::{DAEMON_CREDENTIAL_ENV, run_bounded};
use marshal_launch::tmux::session_key;
use marshal_launch::{LaunchRequest, Launched, PROTOCOL_VERSION, check_shim_version};
use serde::Deserialize;
use serde_json::{Value, json};

/// The daemon's hook port. The shim derives it from the launch address the
/// same way (`ws://host:6155` → `http://host:6156`).
pub const HOOK_PORT: u16 = 6156;
/// This adapter's version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Begin/end markers `marshal-shim codex-setup` writes around its block in
/// Codex's `config.toml`.
const CODEX_BEGIN: &str = "# >>> marshal (managed by marshal-shim codex-setup) >>>";
const CODEX_END: &str = "# <<< marshal (managed by marshal-shim codex-setup) <<<";

/// `stop` and `status` requests.
#[derive(Debug, Deserialize)]
pub struct HandleRequest {
    pub version: u32,
    pub handle: String,
}

/// Run one verb: `input` is the JSON read from stdin, the result is the JSON
/// to write to stdout.
pub fn run(verb: &str, input: &str) -> Result<Value> {
    match verb {
        "check" => {
            let req = parse_launch_request(input)?;
            let t = Target::from_request(&req)?;
            check(&t, &req)?;
            Ok(json!({ "version": PROTOCOL_VERSION }))
        }
        "recover" => {
            let req = parse_launch_request(input)?;
            let t = Target::from_request(&req)?;
            Ok(json!({"version": PROTOCOL_VERSION, "handle": t.handle().to_string()}))
        }
        "inspect" => {
            let req = parse_launch_request(input)?;
            let target = Target::from_request(&req)?;
            let launched = inspect(&target)?;
            Ok(json!({"version": PROTOCOL_VERSION, "launched": launched}))
        }
        "launch" => {
            let req = parse_launch_request(input)?;
            let t = Target::from_request(&req)?;
            let l = launch(&t, &req)?;
            let mut v = serde_json::to_value(&l)?;
            v["version"] = json!(PROTOCOL_VERSION);
            Ok(v)
        }
        "stop" | "status" => {
            let req: HandleRequest =
                serde_json::from_str(input).context("reading the request on stdin")?;
            check_version(req.version)?;
            let h = parse_handle(&req.handle)?;
            if verb == "stop" {
                let state = stop(&h)?;
                Ok(json!({ "version": PROTOCOL_VERSION, "state": state }))
            } else {
                let running = status(&h)?;
                Ok(json!({ "version": PROTOCOL_VERSION, "running": running }))
            }
        }
        other => bail!(
            "unknown verb `{other}` (expected recover, check, launch, inspect, stop or status)"
        ),
    }
}

fn check_version(v: u32) -> Result<()> {
    if v != PROTOCOL_VERSION {
        bail!("request is contract version {v}; this adapter speaks version {PROTOCOL_VERSION}");
    }
    Ok(())
}

pub fn parse_launch_request(input: &str) -> Result<LaunchRequest> {
    let req: LaunchRequest = serde_json::from_str(input).context("reading the request on stdin")?;
    check_version(req.version)?;
    Ok(req)
}

/// Where a request goes, and the addresses it must reach from there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// The ssh destination (`host` or `user@host`).
    pub dest: String,
    /// The operator-facing launch label.
    pub name: String,
    /// The tmux session key for this particular launch.
    pub key: String,
    pub daemon: HostPort,
    pub hook: HostPort,
    /// `--oauth-token-file`: a file on the target holding Claude's long-lived
    /// token. The agent's shell reads it there into CLAUDE_CODE_OAUTH_TOKEN,
    /// so the token never leaves the target or enters a request or log.
    pub token_file: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostPort {
    pub host: String,
    pub port: u16,
}

impl std::fmt::Display for HostPort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.host.contains(':') {
            write!(f, "[{}]:{}", self.host, self.port)
        } else {
            write!(f, "{}:{}", self.host, self.port)
        }
    }
}

impl Target {
    /// Validate a request and work out where it goes. Refuses a loopback
    /// launch address: the agent runs on another machine, where `localhost`
    /// is not the daemon.
    pub fn from_request(req: &LaunchRequest) -> Result<Target> {
        let Options { dest, token_file } = parse_options(&req.options)?;
        validate_name(&req.name)?;
        if req.argv.is_empty() {
            bail!("the request's argv is empty");
        }
        if req.cwd.contains(['\r', '\n']) {
            bail!("the target directory contains a line break");
        }
        for k in req.env.keys() {
            validate_env_key(k)?;
        }
        let addr = req.env.get("MARSHAL_DAEMON_ADDRESS").ok_or_else(|| {
            anyhow!("the request's env has no MARSHAL_DAEMON_ADDRESS (the launch address)")
        })?;
        let daemon =
            ws_host_port(addr).ok_or_else(|| anyhow!("can't read the launch address `{addr}`"))?;
        if is_loopback_host(&daemon.host) {
            bail!(
                "the launch address {addr} is a loopback address, which on {dest} would not be \
                 this daemon; launch with an address the target can reach (a mesh name or IP)"
            );
        }
        // The shim lets MARSHAL_BASE_URL override the hook base; otherwise it
        // is the launch address's host on the hook port.
        let hook = match req.env.get("MARSHAL_BASE_URL").filter(|s| !s.is_empty()) {
            Some(base) => {
                let hp = parse_address(base, &["http://", "https://"], HOOK_PORT)
                    .ok_or_else(|| anyhow!("can't read MARSHAL_BASE_URL `{base}`"))?;
                if is_loopback_host(&hp.host) {
                    bail!("MARSHAL_BASE_URL {base} is a loopback address; {dest} can't reach it");
                }
                hp
            }
            None => HostPort {
                host: daemon.host.clone(),
                port: HOOK_PORT,
            },
        };
        Ok(Target {
            dest,
            name: req.name.clone(),
            key: session_key(&req.agent_id)?,
            daemon,
            hook,
            token_file,
        })
    }
    fn handle(&self) -> Handle {
        Handle {
            dest: self.dest.clone(),
            name: self.key.clone(),
        }
    }
}

/// The ssh adapter's options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// `--host <destination>`, required.
    pub dest: String,
    /// `--oauth-token-file <absolute path>`.
    pub token_file: Option<String>,
}

const OPTIONS_USAGE: &str =
    "the ssh adapter takes --host <destination> and --oauth-token-file <path>";

/// Read the adapter options: `--name value` or `--name=value`.
pub fn parse_options(options: &[String]) -> Result<Options> {
    let (mut host, mut token_file) = (None, None);
    let mut it = options.iter();
    while let Some(o) = it.next() {
        let (name, inline) = match o.split_once('=') {
            Some((n, v)) => (n, Some(v.to_string())),
            None => (o.as_str(), None),
        };
        let slot = match name {
            "--host" => &mut host,
            "--oauth-token-file" => &mut token_file,
            _ => bail!("unknown option `{o}` ({OPTIONS_USAGE})"),
        };
        let value = match inline {
            Some(v) => v,
            None => it
                .next()
                .ok_or_else(|| anyhow!("{name} needs a value"))?
                .clone(),
        };
        *slot = Some(value);
    }
    let dest = host.ok_or_else(|| anyhow!("the ssh adapter needs --host <destination>"))?;
    validate_dest(&dest)?;
    if let Some(v) = &token_file {
        validate_target_path("--oauth-token-file", v)?;
    }
    Ok(Options { dest, token_file })
}

/// A path on the target: absolute, one word.
fn validate_target_path(option: &str, p: &str) -> Result<()> {
    if !p.starts_with('/') || p.chars().any(|c| c.is_whitespace() || c.is_control()) {
        bail!("{option} takes an absolute path on the target with no spaces, not `{p}`");
    }
    Ok(())
}

/// An ssh destination must not be read as an ssh option, and must be one word.
pub fn validate_dest(d: &str) -> Result<()> {
    if d.is_empty()
        || d.starts_with('-')
        || d.chars()
            .any(|c| !(c.is_ascii_alphanumeric() || ".-_@:%[]".contains(c)))
    {
        bail!("`{d}` is not an ssh destination");
    }
    Ok(())
}

/// tmux session names: letters, digits, `-` and `_` (tmux rewrites `.` and
/// `:`, which would make the session hard to find again).
pub fn validate_name(n: &str) -> Result<()> {
    if n.is_empty()
        || n.starts_with('-')
        || !n
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("launch name `{n}` must be letters, digits, `-` and `_`");
    }
    Ok(())
}

fn validate_env_key(k: &str) -> Result<()> {
    let mut cs = k.chars();
    let ok = matches!(cs.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && cs.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !ok {
        bail!("`{k}` is not an environment variable name");
    }
    Ok(())
}

/// Parse `scheme://host[:port][/path]`. IPv6 hosts are bracketed.
pub fn parse_address(addr: &str, schemes: &[&str], default_port: u16) -> Option<HostPort> {
    let rest = schemes.iter().find_map(|s| addr.strip_prefix(s))?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let (host, port) = if let Some(r) = authority.strip_prefix('[') {
        let end = r.find(']')?;
        let host = &r[..end];
        let port = match &r[end + 1..] {
            "" => default_port,
            p => p.strip_prefix(':')?.parse().ok()?,
        };
        (host, port)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h, p.parse().ok()?),
            None => (authority, default_port),
        }
    };
    if host.is_empty() {
        return None;
    }
    Some(HostPort {
        host: host.to_string(),
        port,
    })
}

/// A `ws://` or `wss://` address's host and port, read the way the launcher reads it.
pub fn ws_host_port(addr: &str) -> Option<HostPort> {
    host_port(addr)
        .ok()
        .map(|(host, port)| HostPort { host, port })
}

/// Shell single-quote.
pub fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', r#"'"'"'"#))
}

/// Every script's start: `say` reports a line back. Every script runs in
/// bash (`bash -l -s`), whatever the target's login shell, and quotes its
/// exact-match tmux targets (`"=$name"`): zsh would expand an unquoted
/// `=word` (EQUALS) to a command path. PATH is the target's own login PATH:
/// when the login shell isn't bash (zsh on macOS, where Homebrew's PATH is
/// set in .zprofile), PATH is taken from it, since a harness started by hand
/// there would see that PATH.
const LOGIN: &str = r#"set -u
say() { printf '@@%s\n' "$*"; }
q() { printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"; }
case "${SHELL:-}" in
  ""|*/bash) ;;
  *) if [ -x "$SHELL" ]; then
       p=$("$SHELL" -lc 'printf "%s" "$PATH"' </dev/null 2>/dev/null) && [ -n "$p" ] && PATH=$p
       export PATH
     fi ;;
esac
path=
while IFS= read -r d; do
  case "$d" in /*) path="${path:+$path:}$d" ;; esac
done < <(printf '%s\n' "$PATH" | tr ':' '\n')
PATH=$path
export PATH
"#;

/// Scripts with a folder: `~` is the target's home. `cwd` is resolved the
/// way the shim will see it (`getcwd`, so symlinks resolved).
const PRELUDE: &str = r#"case "$cwd" in "~") cwd=$HOME ;; "~/"*) cwd=$HOME/${cwd#"~/"} ;; esac
"#;

/// Claude's marshal wiring, worked out on the target, through the
/// marshal-shim plugin or by hand as upstream's README documents (an MCP
/// server in `~/.claude.json`, user or this folder's local entry, or the
/// folder's `.mcp.json`; hooks in a `settings.json`). Only facts leave the
/// target: which file, each hook that runs `marshal-shim claude-hook <verb>`
/// (its verb and executable), each MCP server that runs marshal-shim (its
/// executable and any fixed MARSHAL_DAEMON_ADDRESS), and each shim
/// whether executable references match the trusted PATH shim. Other servers, hook arguments and
/// history stay on the target. Reading the JSON needs python3 or jq.
const CLAUDE_FACTS: &str = r#"jfacts() {
  [ -f "$2" ] || return 0
  if command -v python3 >/dev/null 2>&1; then
    python3 -c '
import json, sys
mode, path, cwd = sys.argv[1:4]
try:
    d = json.load(open(path))
except Exception:
    sys.exit(0)
one = lambda s: " ".join(str(s).split())
if mode == "plugins":
    for p in d if isinstance(d, list) else []:
        if isinstance(p, dict) and str(p.get("id", "")).startswith("marshal-shim@"):
            print(("enabled" if p.get("enabled") is True else "disabled") + "\t" + one(p.get("installPath", "")))
elif mode == "hooks":
    def walk(v):
        if isinstance(v, dict):
            for k, x in v.items():
                if k == "command" and isinstance(x, str):
                    print(one(x))
                else:
                    walk(x)
        elif isinstance(v, list):
            for x in v:
                walk(x)
    walk(d.get("hooks") if isinstance(d, dict) else None)
else:
    if mode == "local":
        d = ((d.get("projects") or {}).get(cwd) or {}) if isinstance(d, dict) else {}
    servers = d.get("mcpServers") if isinstance(d, dict) else None
    for n, s in (servers or {}).items() if isinstance(servers, dict) else []:
        if isinstance(s, dict):
            e = s.get("env") if isinstance(s.get("env"), dict) else {}
            print(one(n) + "\t" + one(s.get("command", "")) + "\t" + one(e.get("MARSHAL_DAEMON_ADDRESS", "")))
' "$1" "$2" "$cwd"
  else
    case "$1" in
      plugins) jq -r '.[]? | objects | select((.id // "" | tostring) | startswith("marshal-shim@"))
                 | [(if .enabled == true then "enabled" else "disabled" end), (.installPath // "" | tostring)]
                 | map(gsub("\\s+"; " ")) | join("\t")' "$2" 2>/dev/null ;;
      hooks) jq -r '.hooks? // {} | .. | objects | .command? | strings | gsub("\\s+"; " ")' "$2" 2>/dev/null ;;
      *) jq -r --arg m "$1" --arg c "$cwd" '(if $m == "local" then (.projects[$c]? // {}) else . end)
           | .mcpServers? // {} | to_entries[]? | select(.value | type == "object")
           | [.key, (.value.command // "" | tostring), (.value.env.MARSHAL_DAEMON_ADDRESS? // "" | tostring)]
           | map(gsub("\\s+"; " ")) | join("\t")' "$2" 2>/dev/null ;;
    esac
  fi
}
is_shim() { case "${1##*/}" in marshal-shim|marshal-shim.exe) return 0 ;; esac; return 1; }
unq() { u=${1#[\"\']}; printf '%s' "${u%[\"\']}"; }
tab=$(printf '\t')
hook_facts() {
  w=$1
  jfacts hooks "$2" | while IFS= read -r c; do
    set -f; set -- $c; set +f
    e=$(unq "${1:-}")
    if is_shim "$e" && [ "${2:-}" = claude-hook ] && [ -n "${3:-}" ]; then
      printf '@@claude-hook %s\t%s\t%s\n' "$3" "$e" "$w"
    fi
  done
}
# mcp_facts <mode> <scope> <where> <file>: each marshal-shim server, with
# its executable and any fixed address.
mcp_facts() {
  scope=$2; w=$3
  jfacts "$1" "$4" | while IFS="$tab" read -r n c a; do
    set -f; set -- $c; set +f
    e=$(unq "${1:-}")
    if is_shim "$e"; then
      case "$a" in *'${MARSHAL_DAEMON_ADDRESS'*) a= ;; esac
      printf '@@claude-mcp %s\t%s\t%s\t%s\t%s\n' "$scope" "$n" "$e" "$a" "$w"
    fi
  done
}
claude_facts() {
  if ! command -v python3 >/dev/null 2>&1 && ! command -v jq >/dev/null 2>&1; then
    say "no-json-reader"
    return 0
  fi
  if command -v claude >/dev/null 2>&1; then
    list=$(mktemp)
    (cd "$HOME" && claude plugin list --json) > "$list" 2>/dev/null
    jfacts plugins "$list" | while IFS="$tab" read -r state p; do
      printf '@@claude-plugin %s\t%s\n' "$state" "$p"
      if [ "$state" = enabled ]; then
        hook_facts "$p/hooks/hooks.json" "$p/hooks/hooks.json"
        mcp_facts mcpServers plugin "$p/.mcp.json" "$p/.mcp.json"
      fi
    done
    rm -f "$list"
  fi
  for f in "$HOME/.claude/settings.json" "$cwd/.claude/settings.json" \
           "$cwd/.claude/settings.local.json" /etc/claude-code/managed-settings.json; do
    hook_facts "$f" "$f"
  done
  mcp_facts mcpServers user "~/.claude.json" "$HOME/.claude.json"
  mcp_facts local local "~/.claude.json for $cwd" "$HOME/.claude.json"
  mcp_facts mcpServers project "$cwd/.mcp.json" "$cwd/.mcp.json"
}
claude_wiring() {
  facts=$(claude_facts)
  printf '%s\n' "$facts"
  # Each shim the wiring runs, as the harness will find it (a bare
  # marshal-shim through the login PATH), and its version.
  printf '%s\n' "$facts" | awk -F'\t' '
    /^@@claude-hook / { print $2 }
    /^@@claude-mcp / { print $3 }' \
    | sort -u | while IFS= read -r e; do
        trusted=0
        case "$e" in
          marshal-shim) [ -n "$trusted_shim" ] && trusted=1 ;;
          /*) [ -n "$trusted_shim" ] && [ "$e" -ef "$trusted_shim" ] && trusted=1 ;;
        esac
        if [ "$trusted" -ne 1 ]; then
          printf '@@untrusted-shim %s\n' "$e"
        fi
      done
}
"#;

const CODEX_FACTS: &str = r#"codex_facts() {
  awk -v begin="$codex_begin" -v end="$codex_end" -v address="$req_address" -v hook_base="$expected_hook" '
    BEGIN {
      sub(/\/$/, "", hook_base)
      if (hook_base !~ /^http:\/\/[^\/?#]+$/) hook_mismatch=1
    }
    $0 == begin { inside=1; next }
    inside && $0 == end { complete=1; exit }
    inside && /^ *\[/ { mcp_section=($0 == "[mcp_servers.marshal]") }
    inside && mcp_section && /^command[[:space:]]*=/ && /marshal-shim"/ { mcp=1 }
    inside && /^[[:space:]]*command[[:space:]]*=/ && /marshal-shim.* codex-hook / {
      if ($0 ~ / codex-hook session-start /) hook=1
      endpoint=$0
      sub(/^.* codex-hook [^[:space:]]+ /, "", endpoint)
      sub(/"$/, "", endpoint)
      sub(/^\047|\047$/, "", endpoint)
      sub(/^\"|\"$/, "", endpoint)
      sub(/\/$/, "", endpoint)
      if (endpoint != hook_base) hook_mismatch=1
    }
    inside && mcp_section && /MARSHAL_DAEMON_ADDRESS[[:space:]]*=/ {
      value=$0
      sub(/^.*MARSHAL_DAEMON_ADDRESS[[:space:]]*=[[:space:]]*"/, "", value)
      sub(/".*$/, "", value)
      mismatch=(value != address)
    }
    END {
      if (complete)
        printf "@@codex-managed yes\n@@codex-mcp %s\n@@codex-hooks %s\n@@codex-daemon %s\n@@codex-hook-address %s\n",
          mcp ? "yes" : "no", hook ? "yes" : "no", mismatch ? "mismatch" : "match",
          hook_mismatch ? "mismatch" : "match"
    }
  ' "$1"
}
"#;

/// The `check` script: reports each probe, starts nothing.
pub fn check_script(t: &Target, req: &LaunchRequest) -> String {
    let mut s = format!(
        "daemon_host={}\ndaemon_port={}\nhook_host={}\nhook_port={}\ncwd={}\nharness={}\nargv0={}\nreq_xdg={}\nreq_base={}\ntoken_file={}\nreq_address={}\ncodex_begin={}\ncodex_end={}\n",
        sq(&t.daemon.host),
        t.daemon.port,
        sq(&t.hook.host),
        t.hook.port,
        sq(&req.cwd),
        sq(&req.harness),
        sq(&req.argv[0]),
        sq(req
            .env
            .get("XDG_CONFIG_HOME")
            .map(String::as_str)
            .unwrap_or("")),
        sq(req
            .env
            .get("MARSHAL_BASE_URL")
            .map(String::as_str)
            .unwrap_or("")),
        sq(t.token_file.as_deref().unwrap_or("")),
        sq(req
            .env
            .get("MARSHAL_DAEMON_ADDRESS")
            .map(String::as_str)
            .unwrap_or("")),
        sq(CODEX_BEGIN),
        sq(CODEX_END),
    );
    s.push_str(LOGIN);
    s.push_str(PRELUDE);
    s.push_str(CLAUDE_FACTS);
    s.push_str(CODEX_FACTS);
    s.push_str(
        r#"probe() {
  if command -v timeout >/dev/null 2>&1; then
    timeout 5 bash -c ': </dev/tcp/"$1"/"$2"' probe "$1" "$2" 2>/dev/null
  else
    bash -c ': </dev/tcp/"$1"/"$2"' probe "$1" "$2" 2>/dev/null
  fi
}
say "host $(uname -n)"
# Without one in the request, the shim there takes MARSHAL_BASE_URL from the
# target's own environment, so probe the hook listener that names.
if [ -z "$req_base" ] && [ -n "${MARSHAL_BASE_URL:-}" ]; then
  a=${MARSHAL_BASE_URL#*://}; a=${a%%/*}
  case "$MARSHAL_BASE_URL" in https://*) dp=443 ;; *) dp=80 ;; esac
  case "$a" in
    \[*\]:*) hook_host=${a%%]:*}; hook_host=${hook_host#[}; hook_port=${a##*]:} ;;
    \[*\]) hook_host=${a#[}; hook_host=${hook_host%]}; hook_port=$dp ;;
    *:*) hook_host=${a%:*}; hook_port=${a##*:} ;;
    *) hook_host=$a; hook_port=$dp ;;
  esac
  say "hook-from-target $hook_host $hook_port"
fi
if probe "$daemon_host" "$daemon_port"; then say "daemon-port ok"; else say "daemon-port fail"; fi
if probe "$hook_host" "$hook_port"; then say "hook-port ok"; else say "hook-port fail"; fi
trusted_shim=$(command -v marshal-shim 2>/dev/null) || trusted_shim=
case "$trusted_shim" in
  /*) if [ -x "$trusted_shim" ]; then say "shim $("$trusted_shim" --version 2>&1 | head -n 1)"; fi ;;
  *) trusted_shim= ;;
esac
if cd "$cwd" 2>/dev/null; then cwd=$(pwd -P); say "cwd $cwd"; else say "cwd-missing $cwd"; fi
if command -v tmux >/dev/null 2>&1; then say "tmux ok"; else say "tmux missing"; fi
if command -v "$argv0" >/dev/null 2>&1; then say "harness ok"; else say "harness missing"; fi
# Only whether the token file can be read and isn't empty, never its contents.
if [ -n "$token_file" ]; then
  if [ -r "$token_file" ] && [ -s "$token_file" ]; then say "token-file ok"; else say "token-file missing"; fi
fi
# The shim reads marshal/daemon-address ahead of MARSHAL_DAEMON_ADDRESS: the
# first of these with a non-empty first line wins. The agent's XDG_CONFIG_HOME
# is the request's if it sets one.
xdg=${req_xdg:-${XDG_CONFIG_HOME:-}}
for f in ${xdg:+"$xdg/marshal/daemon-address"} "$HOME/.config/marshal/daemon-address"; do
  [ -f "$f" ] || continue
  line=$(head -n 1 "$f" | tr -d '\r' | sed 's/^[[:space:]]*//; s/[[:space:]]*$//')
  [ -n "$line" ] || continue
  say "daemon-address-file $f"
  say "daemon-address $line"
  break
done
case "$harness" in
claude)
  claude_wiring
  ;;
codex)
  cfg="${CODEX_HOME:-$HOME/.codex}/config.toml"
  if [ -f "$cfg" ]; then
    expected_hook=${req_base:-${MARSHAL_BASE_URL:-}}
    if [ -z "$expected_hook" ]; then
      case "$hook_host" in *:*) hp="[$hook_host]" ;; *) hp=$hook_host ;; esac
      expected_hook=http://$hp:$hook_port
    fi
    codex_facts "$cfg"
  else
    say "codex-config-missing $cfg"
  fi
  ;;
esac
say "done"
"#,
    );
    s
}

/// A Claude hook that runs `marshal-shim claude-hook <verb>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeHook {
    pub verb: String,
    pub exe: String,
    /// The file it's in.
    pub from: String,
}

/// A Claude MCP server that runs marshal-shim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeMcp {
    /// `plugin`, `local` (this folder's entry in ~/.claude.json), `project`
    /// (the folder's .mcp.json) or `user` (~/.claude.json).
    pub scope: String,
    pub name: String,
    /// Its marshal-shim executable, as written there.
    pub exe: String,
    /// A MARSHAL_DAEMON_ADDRESS fixed in its config, if any.
    pub fixed_address: Option<String>,
    /// The file it's in.
    pub from: String,
}

/// What the `check` script reported.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Probe {
    pub host: Option<String>,
    /// `uname -s` on the target, reported by `launch`.
    pub os: Option<String>,
    /// The tmux `launch` ran, as the target's login PATH resolved it.
    pub tmux_path: Option<String>,
    pub daemon_port_ok: Option<bool>,
    pub hook_port_ok: Option<bool>,
    pub cwd: Option<String>,
    pub cwd_missing: Option<String>,
    pub tmux: Option<bool>,
    pub harness: Option<bool>,
    /// `marshal-shim --version` output; `None` if not on PATH.
    pub shim: Option<String>,
    pub codex_managed: bool,
    pub codex_mcp: bool,
    pub codex_hooks: bool,
    pub codex_daemon_matches: bool,
    pub codex_hook_matches: bool,
    /// The marshal-shim plugin, if Claude lists it: enabled, and its path.
    pub claude_plugin: Option<(bool, String)>,
    /// Each Claude hook that runs `marshal-shim claude-hook <verb>`.
    pub claude_hooks: Vec<ClaudeHook>,
    /// Each Claude MCP server that runs marshal-shim.
    pub claude_mcp: Vec<ClaudeMcp>,
    pub untrusted_shims: Vec<String>,
    pub codex_config_missing: Option<String>,
    /// The hook listener the target's own `MARSHAL_BASE_URL` names, when the
    /// request sets none: the shim there uses it, so the check probes it.
    pub hook_from_target: Option<HostPort>,
    /// Neither python3 nor jq on the target, so hand-wired Claude settings
    /// couldn't be read.
    pub no_json_reader: bool,
    /// The shim's `marshal/daemon-address` file, if one names an address:
    /// its path and that address.
    pub daemon_address_file: Option<(String, String)>,
    /// `--oauth-token-file` is readable and not empty there (never its contents).
    pub token_file: Option<bool>,
    pub done: bool,
}

impl Probe {
    pub fn parse(out: &str) -> Probe {
        let mut p = Probe::default();
        for line in out.lines() {
            let Some(l) = line.strip_prefix("@@") else {
                continue;
            };
            let (k, v) = l.split_once(' ').unwrap_or((l, ""));
            match k {
                "host" => p.host = Some(v.to_string()),
                "os" => p.os = Some(v.to_string()),
                "tmux-path" => p.tmux_path = Some(v.to_string()),
                "daemon-port" => p.daemon_port_ok = Some(v == "ok"),
                "hook-port" => p.hook_port_ok = Some(v == "ok"),
                "cwd" => p.cwd = Some(v.to_string()),
                "cwd-missing" => p.cwd_missing = Some(v.to_string()),
                "tmux" => p.tmux = Some(v == "ok"),
                "harness" => p.harness = Some(v == "ok"),
                "token-file" => p.token_file = Some(v == "ok"),
                "shim" => p.shim = Some(v.to_string()),
                "codex-config-missing" => p.codex_config_missing = Some(v.to_string()),
                "no-json-reader" => p.no_json_reader = true,
                "claude-plugin" => {
                    if let Some((state, path)) = v.split_once('\t') {
                        p.claude_plugin = Some((state == "enabled", path.to_string()));
                    }
                }
                "claude-hook" => {
                    let f: Vec<&str> = v.splitn(3, '\t').collect();
                    if let [verb, exe, from] = f[..] {
                        p.claude_hooks.push(ClaudeHook {
                            verb: verb.into(),
                            exe: exe.into(),
                            from: from.into(),
                        });
                    }
                }
                "claude-mcp" => {
                    let f: Vec<&str> = v.splitn(5, '\t').collect();
                    if let [scope, name, exe, addr, from] = f[..] {
                        p.claude_mcp.push(ClaudeMcp {
                            scope: scope.into(),
                            name: name.into(),
                            exe: exe.into(),
                            fixed_address: (!addr.is_empty()).then(|| addr.to_string()),
                            from: from.into(),
                        });
                    }
                }
                "untrusted-shim" => p.untrusted_shims.push(v.into()),
                "codex-managed" => p.codex_managed = v == "yes",
                "codex-mcp" => p.codex_mcp = v == "yes",
                "codex-hooks" => p.codex_hooks = v == "yes",
                "codex-daemon" => p.codex_daemon_matches = v == "match",
                "codex-hook-address" => p.codex_hook_matches = v == "match",
                "hook-from-target" => {
                    p.hook_from_target = v.split_once(' ').and_then(|(h, port)| {
                        Some(HostPort {
                            host: h.to_string(),
                            port: port.parse().ok()?,
                        })
                    })
                }
                "daemon-address-file" => {
                    p.daemon_address_file = Some((v.to_string(), String::new()))
                }
                "daemon-address" => {
                    if let Some((_, a)) = &mut p.daemon_address_file {
                        *a = v.to_string();
                    }
                }
                "done" => p.done = true,
                _ => {}
            }
        }
        p
    }

    /// Every reason this target can't run `req`, in the order a person would
    /// fix them. Empty means it can.
    pub fn failures(&self, t: &Target, req: &LaunchRequest) -> Vec<String> {
        let host = self.host.as_deref().unwrap_or(&t.dest);
        let mut f = Vec::new();
        if !self.done {
            f.push(format!("the check on {host} stopped before it finished"));
            return f;
        }
        if self.daemon_port_ok != Some(true) {
            f.push(format!(
                "{host} can't reach the daemon at {} (the launch address), so the agent could not \
                 join. Check that the daemon's MARSHAL_BIND listens on an address {host} can reach",
                t.daemon
            ));
        }
        if self.hook_port_ok != Some(true) {
            f.push(format!(
                "{host} can't reach the daemon's hook listener at {}, so the agent's hooks could \
                 not register it or deliver its mail. Check that the daemon's MARSHAL_HOOK_BIND \
                 listens on an address {host} can reach (it listens on loopback unless set)",
                self.hook_from_target.as_ref().unwrap_or(&t.hook)
            ));
        }
        if let Some(c) = &self.cwd_missing {
            f.push(format!("{host} has no folder {c}"));
        }
        if self.tmux != Some(true) {
            f.push(format!("{host} has no tmux on its login PATH"));
        }
        if self.harness != Some(true) {
            f.push(format!(
                "{host} has no `{}` on PATH (in a login shell)",
                req.argv[0]
            ));
        }
        if let Some(path) = &t.token_file
            && self.token_file != Some(true)
        {
            f.push(format!(
                "{host} has no readable, non-empty token file at {path} (--oauth-token-file)"
            ));
        }
        match &self.shim {
            None => f.push(format!("{host} has no marshal-shim on its login PATH")),
            Some(v) => {
                if let Err(e) = check_shim_version(v, &req.marshal_version) {
                    f.push(format!("on {host}: {e:#}"));
                }
            }
        }
        if let Some((path, named)) = &self.daemon_address_file
            && ws_host_port(named).as_ref() != Some(&t.daemon)
        {
            f.push(format!(
                "{host} has {path} naming {named}; the shim reads that file ahead of \
                 MARSHAL_DAEMON_ADDRESS, so the agent would join that daemon and not the launch \
                 address {}. Remove the file or make it name the launch address",
                t.daemon
            ));
        }
        match req.harness.as_str() {
            "claude" => f.extend(self.claude_wiring(host, t)),
            "codex" => f.extend(self.codex_wiring(host, t)),
            other => f.push(format!(
                "the ssh adapter can't check the wiring of harness `{other}` (it knows claude and codex)"
            )),
        }
        f
    }

    /// Claude is wired when some MCP server it loads runs marshal-shim and
    /// its hooks run `marshal-shim claude-hook`, including `wait`, through
    /// the marshal-shim plugin or by hand; and every shim they name by path
    /// is there at this launch's major.minor.
    fn claude_wiring(&self, host: &str, t: &Target) -> Vec<String> {
        let mut notes: Vec<String> = Vec::new();
        if matches!(self.claude_plugin, Some((false, _))) {
            notes.push("the marshal-shim plugin is installed but not enabled".into());
        }
        if self.no_json_reader {
            notes.push("it has neither python3 nor jq to read Claude's settings files".into());
        }
        let why = if notes.is_empty() {
            String::new()
        } else {
            format!(" ({})", notes.join("; "))
        };

        let mut f = Vec::new();
        if self.claude_mcp.is_empty() {
            f.push(format!(
                "Claude on {host} has no MCP server that runs marshal-shim: none in the \
                 marshal-shim plugin, ~/.claude.json or the folder's .mcp.json{why}"
            ));
        }
        for m in &self.claude_mcp {
            // A fixed address elsewhere would send the agent to another daemon.
            if let Some(a) = &m.fixed_address
                && ws_host_port(a).as_ref() != Some(&t.daemon)
            {
                f.push(format!(
                    "Claude's marshal-shim MCP server on {host} ({}) is fixed to {a}, not the \
                     launch address {}",
                    m.from, t.daemon
                ));
            }
        }
        if self.claude_hooks.is_empty() {
            f.push(format!(
                "Claude's hooks on {host} don't run `marshal-shim claude-hook`: none in the \
                 marshal-shim plugin or a settings.json, so a launched agent would not get its \
                 mail{why}"
            ));
        } else if !self.claude_hooks.iter().any(|h| h.verb == "wait") {
            f.push(format!(
                "Claude's hooks on {host} run marshal-shim but not `marshal-shim claude-hook \
                 wait`, which wakes an idle agent when its first task arrives. Add it as an \
                 asyncRewake hook on SessionStart and Stop, as the marshal-shim plugin does"
            ));
        }
        for executable in &self.untrusted_shims {
            f.push(format!("Claude's marshal wiring on {host} references {executable:?}, outside the trusted marshal-shim installation on PATH"));
        }
        f
    }

    fn codex_wiring(&self, host: &str, t: &Target) -> Vec<String> {
        if let Some(cfg) = &self.codex_config_missing {
            return vec![format!(
                "Codex on {host} has no {cfg}; run `marshal-shim codex-setup` there"
            )];
        }
        if !self.codex_managed {
            return vec![format!(
                "Codex on {host} has no complete marshal block in config.toml; run `marshal-shim codex-setup` there"
            )];
        }
        let mut f = Vec::new();
        if !self.codex_mcp {
            f.push(format!(
                "Codex's marshal MCP server on {host} is not marshal-shim"
            ));
        }
        if !self.codex_hooks {
            f.push(format!(
                "Codex's hooks on {host} don't run `marshal-shim codex-hook`"
            ));
        }
        if self.codex_hooks && !self.codex_hook_matches {
            f.push(format!("Codex's hooks on {host} target a different hook URL; rerun `marshal-shim codex-setup` with the launch hook URL"));
        }
        if !self.codex_daemon_matches {
            f.push(format!("Codex on {host} is configured for a different daemon; run `marshal-shim codex-setup` with the launch address {}", t.daemon));
        }
        f
    }
}

/// The `launch` script: start `argv` in a detached tmux session, in `cwd`,
/// with `env` exported, through a login shell so PATH is the target's own.
/// Adds nothing to `argv`.
pub fn launch_script(t: &Target, req: &LaunchRequest) -> String {
    let mut inner = format!(
        "unset MARSHAL_AGENT_ID MARSHAL_LAUNCH_ID {}\n",
        DAEMON_CREDENTIAL_ENV.join(" ")
    );
    for (k, v) in req
        .env
        .iter()
        .filter(|(key, _)| !DAEMON_CREDENTIAL_ENV.contains(&key.as_str()))
    {
        inner.push_str(&format!("export {k}={}\n", sq(v)));
    }
    // Read on the target when the agent starts: the script, the tmux command
    // and the request carry only the path.
    if let Some(f) = &t.token_file {
        inner.push_str(&format!(
            "CLAUDE_CODE_OAUTH_TOKEN=$(cat {}) || exit 1\nexport CLAUDE_CODE_OAUTH_TOKEN\n",
            sq(f)
        ));
    }
    inner.push_str("exec");
    for a in &req.argv {
        inner.push(' ');
        inner.push_str(&sq(a));
    }
    inner.push('\n');
    let mut s = format!(
        "name={}\ncwd={}\ninner={}\ntoken_file={}\n",
        sq(&t.key),
        sq(&req.cwd),
        sq(&inner),
        sq(t.token_file.as_deref().unwrap_or(""))
    );
    s.push_str(LOGIN);
    s.push_str(PRELUDE);
    s.push_str(TMUX_SESSION);
    s.push_str(
        r#"if locate_session; then
  report_session
  exit $?
else
  rc=$?
  [ "$rc" -eq 1 ] || exit "$rc"
fi
h=$(uname -n)
cd "$cwd" 2>/dev/null || { echo "$h has no folder $cwd" >&2; exit 1; }
cwd=$(pwd -P)
case "$cwd" in *$'\n'*|*$'\r'*) echo "the resolved target directory contains a line break" >&2; exit 1 ;; esac
if [ -n "$token_file" ] && ! { [ -r "$token_file" ] && [ -s "$token_file" ]; }; then
  echo "$h has no readable, non-empty token file at $token_file" >&2; exit 1
fi
inner="export PATH=$(q "$PATH")
$inner"
tmux new-session -d -s "$name" -c "$cwd" \
    -e "MARSHAL_LAUNCH_KEY=$name" -e "MARSHAL_LAUNCH_CWD=$cwd" -e "MARSHAL_LAUNCH_HOST=$h" \
    bash -lc "$inner" || exit 1
report_values "$h" "$cwd"
"#,
    );
    s
}

const TMUX_SESSION: &str = r#"locate_session() {
  listing=$(LC_ALL=C tmux list-sessions -F '#{session_id} #{session_name}' 2>&1) || {
    case "$listing" in
      'no server running on '*|'error connecting to '*' (No such file or directory)') return 1 ;;
      *) echo "$listing" >&2; return 2 ;;
    esac
  }
  session_id=$(printf '%s\n' "$listing" | awk -v name="$name" 'substr($0, length($1) + 2) == name { print $1 }')
  [ -n "$session_id" ] || return 1
  marker=$(tmux show-environment -t "$session_id" MARSHAL_LAUNCH_KEY) || return 2
  if [ "$marker" != "MARSHAL_LAUNCH_KEY=$name" ]; then
    echo "tmux session $name does not identify this launch" >&2
    return 2
  fi
}
report_values() {
  say "host $1"
  say "os $(uname -s)"
  say "tmux-path $(command -v tmux)"
  say "cwd $2"
  say "done"
}
report_session() {
  location=$(tmux show-environment -t "$session_id" MARSHAL_LAUNCH_CWD) || return 2
  launch_host=$(tmux show-environment -t "$session_id" MARSHAL_LAUNCH_HOST) || return 2
  report_values "${launch_host#MARSHAL_LAUNCH_HOST=}" "${location#MARSHAL_LAUNCH_CWD=}"
}
"#;

fn session_script(name: &str, action: &str, absent: &str) -> String {
    format!(
        "name={}\n{LOGIN}{TMUX_SESSION}if locate_session; then\n{action}\nelse\n  rc=$?\n  [ \"$rc\" -eq 1 ] || exit \"$rc\"\n{absent}\nfi\n",
        sq(name)
    )
}

pub fn stop_script(name: &str) -> String {
    session_script(
        name,
        r##"  result=$(tmux if-shell -F -t "$session_id" "#{==:#{MARSHAL_LAUNCH_KEY},$name}" \
    "kill-session -t '$session_id' ; display-message -p marshal-stopped" \
    'display-message -p marshal-mismatch') || exit 2
  if [ "$result" != marshal-stopped ]; then
    echo "tmux did not confirm stopping $name" >&2
    exit 2
  fi
  say "stop stopped""##,
        r#"  say "stop gone""#,
    )
}

pub fn status_script(name: &str) -> String {
    session_script(name, r#"  say "running true""#, r#"  say "running false""#)
}

fn inspect_script(name: &str) -> String {
    session_script(name, "  report_session", r#"  say "running false""#)
}

/// What a handle names: `<destination> <session>`, both single words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handle {
    pub dest: String,
    pub name: String,
}

impl std::fmt::Display for Handle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.dest, self.name)
    }
}

pub fn parse_handle(h: &str) -> Result<Handle> {
    let (dest, name) = h
        .split_once(' ')
        .ok_or_else(|| anyhow!("`{h}` is not an ssh adapter handle"))?;
    validate_dest(dest)?;
    let agent_id = name
        .strip_prefix("marshal-")
        .ok_or_else(|| anyhow!("invalid ssh recovery handle"))?;
    session_key(agent_id)?;
    Ok(Handle {
        dest: dest.into(),
        name: name.into(),
    })
}

/// Run `script` with `bash -l -s` on `dest`. stderr passes through to the
/// user; stdout is returned. Fails if ssh can't reach `dest` (exit 255) or the
/// script exits nonzero.
pub fn remote(dest: &str, script: &str) -> Result<String> {
    let ssh = std::env::var("MARSHAL_LAUNCH_SSH").unwrap_or_else(|_| "ssh".to_string());
    let mut words = ssh.split_whitespace();
    let prog = words
        .next()
        .ok_or_else(|| anyhow!("MARSHAL_LAUNCH_SSH is empty"))?;
    let mut command = Command::new(prog);
    command.args(words).args([
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=7",
        "-o",
        "ServerAliveInterval=4",
        "-o",
        "ServerAliveCountMax=2",
        "--",
        dest,
        "bash -l -s",
    ]);
    let out = run_bounded(command, script.as_bytes().to_vec(), Duration::from_secs(15))
        .with_context(|| format!("running remote command on {dest}"))?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !stderr.is_empty() {
        eprint!("{stderr}");
    }
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    match out.status.code() {
        Some(0) => Ok(stdout),
        Some(255) => bail!("ssh to {dest} did not return a result"),
        Some(c) => bail!("the script on {dest} failed (exit {c})"),
        None => bail!("ssh to {dest} was killed by a signal"),
    }
}

pub fn check(t: &Target, req: &LaunchRequest) -> Result<()> {
    let out = remote(&t.dest, &check_script(t, req))?;
    let failures = Probe::parse(&out).failures(t, req);
    if failures.is_empty() {
        return Ok(());
    }
    let mut msg = format!("{} can't run this launch:", t.dest);
    for f in &failures {
        msg.push_str("\n  - ");
        msg.push_str(f);
    }
    Err(anyhow!(msg))
}

/// Printed when `launch` starts an agent on macOS, where Local Network
/// privacy can drop the shim's connections with no other sign.
pub const MACOS_NOTE: &str = "note: on macOS, Local Network privacy can silently drop the shim's \
     connections to the daemon when the agent runs in a tmux server started over ssh. Start a tmux \
     server from the desktop session first (in Terminal, `tmux new-session -s desktop`); if the agent \
     doesn't join, check the target's kernel log for `reason: NECP`; see \"macOS targets\" in \
     docs/agent-launcher.md";

pub fn launch(t: &Target, req: &LaunchRequest) -> Result<Launched> {
    let out = remote(&t.dest, &launch_script(t, req))?;
    let probe = Probe::parse(&out);
    if probe.os.as_deref() == Some("Darwin") {
        eprintln!("{MACOS_NOTE}");
    }
    observation(t, probe)
}

fn inspect(t: &Target) -> Result<Option<Launched>> {
    let out = remote(&t.dest, &inspect_script(&t.key))?;
    if receipt(&out, "@@running ") == Some("false") {
        return Ok(None);
    }
    observation(t, Probe::parse(&out)).map(Some)
}

fn observation(t: &Target, p: Probe) -> Result<Launched> {
    let (Some(host), Some(cwd), true) = (p.host, p.cwd, p.done) else {
        bail!(
            "the script on {} did not report the launch location",
            t.dest
        );
    };
    Ok(Launched {
        handle: t.handle().to_string(),
        r#where: format!("{host} tmux:{}", t.name),
        attach: Some(attach_command(&t.dest, p.tmux_path.as_deref(), &t.key)),
        host,
        cwd,
    })
}

/// The command a user runs to watch the agent. ssh runs it in the target's
/// non-login shell, whose PATH may not hold tmux (Homebrew's on macOS is set
/// only in the login profile), so it names the tmux `launch` used by its full
/// path. The target is quoted: zsh expands an unquoted `=word` (EQUALS).
pub fn attach_command(dest: &str, tmux_path: Option<&str>, name: &str) -> String {
    let tmux = match tmux_path {
        Some(p) if p.starts_with('/') => sq(p),
        _ => "tmux".to_string(),
    };
    let remote = format!("{tmux} attach -t {}", sq(&format!("={name}")));
    format!("ssh -t -- {} {}", sq(dest), sq(&remote))
}

pub fn stop(h: &Handle) -> Result<&'static str> {
    let out = remote(&h.dest, &stop_script(&h.name))?;
    match receipt(&out, "@@stop ") {
        Some("stopped") => Ok("stopped"),
        Some("gone") => Ok("gone"),
        _ => bail!("the stop script on {} did not confirm the outcome", h.dest),
    }
}

pub fn status(h: &Handle) -> Result<bool> {
    let out = remote(&h.dest, &status_script(&h.name))?;
    match receipt(&out, "@@running ") {
        Some("true") => Ok(true),
        Some("false") => Ok(false),
        _ => bail!("the status script on {} did not answer", h.dest),
    }
}

fn receipt<'a>(out: &'a str, prefix: &str) -> Option<&'a str> {
    let mut matches = out.lines().filter_map(|line| line.strip_prefix(prefix));
    let first = matches.next()?;
    matches.next().is_none().then_some(first)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn attach_keeps_remote_quoting_after_local_shell_parsing() {
        for (path, expected) in [
            (
                Some("/Applications/My Tools/tmux"),
                "'/Applications/My Tools/tmux' attach -t '=n'",
            ),
            (None, "tmux attach -t '=n'"),
        ] {
            let attach = attach_command("h", path, "n");
            let output = Command::new("sh")
                .args([
                    "-c",
                    &format!(r#"ssh() {{ shift 3; [ "$#" = 1 ] && printf '%s' "$1"; }}; {attach}"#),
                ])
                .output()
                .unwrap();
            assert!(output.status.success());
            assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
        }
        assert!(validate_dest("host;echo").is_err());
    }

    fn req(addr: &str) -> LaunchRequest {
        LaunchRequest {
            version: PROTOCOL_VERSION,
            harness: "claude".into(),
            argv: vec!["claude".into()],
            cwd: "~/work/app".into(),
            name: "claude-app".into(),
            agent_id: "launch-1".into(),
            marshal_version: VERSION.into(),
            env: BTreeMap::from([
                ("MARSHAL_AGENT_ID".into(), "launch-1".into()),
                ("MARSHAL_DAEMON_ADDRESS".into(), addr.into()),
            ]),
            options: vec!["--host".into(), "build-03".into()],
        }
    }

    #[test]
    fn refuses_a_loopback_launch_address() {
        for a in [
            "ws://localhost:6155",
            "ws://127.0.0.1:6155",
            "ws://127.9.9.9",
            "ws://[::1]:6155",
            "ws://foo.localhost:6155",
            "ws://[::ffff:127.0.0.1]:6155",
        ] {
            let e = Target::from_request(&req(a)).unwrap_err().to_string();
            assert!(e.contains("loopback"), "{a}: {e}");
        }
        let t = Target::from_request(&req("ws://marshal.mesh:6155")).unwrap();
        assert_eq!(t.dest, "build-03");
        assert_eq!(
            t.daemon,
            HostPort {
                host: "marshal.mesh".into(),
                port: 6155
            }
        );
        assert_eq!(
            t.hook,
            HostPort {
                host: "marshal.mesh".into(),
                port: 6156
            }
        );
    }

    #[test]
    fn hook_port_follows_marshal_base_url_and_refuses_its_loopback() {
        let mut r = req("ws://100.86.1.2:6155");
        r.env
            .insert("MARSHAL_BASE_URL".into(), "http://hooks.mesh:7000/".into());
        let t = Target::from_request(&r).unwrap();
        assert_eq!(t.daemon.port, 6155);
        assert_eq!(
            t.hook,
            HostPort {
                host: "hooks.mesh".into(),
                port: 7000
            }
        );
        r.env
            .insert("MARSHAL_BASE_URL".into(), "http://localhost:6156".into());
        assert!(
            Target::from_request(&r)
                .unwrap_err()
                .to_string()
                .contains("loopback")
        );
    }

    #[test]
    fn refuses_bad_requests() {
        let mut r = req("ws://m:6155");
        r.env.remove("MARSHAL_DAEMON_ADDRESS");
        assert!(
            Target::from_request(&r)
                .unwrap_err()
                .to_string()
                .contains("MARSHAL_DAEMON_ADDRESS")
        );
        let mut r = req("ws://m:6155");
        r.options = vec![];
        assert!(
            Target::from_request(&r)
                .unwrap_err()
                .to_string()
                .contains("--host")
        );
        r.options = vec!["--host".into(), "-oProxyCommand=x".into()];
        assert!(Target::from_request(&r).is_err());
        r.options = vec!["--host=a b".into()];
        assert!(Target::from_request(&r).is_err());
        let mut r = req("ws://m:6155");
        r.name = "a.b".into();
        assert!(Target::from_request(&r).is_err());
        let mut r = req("ws://m:6155");
        r.env.insert("A;B".into(), "x".into());
        assert!(Target::from_request(&r).is_err());
        assert!(parse_launch_request(r#"{"version":2}"#).is_err());
    }

    #[test]
    fn options() {
        let o = |v: &[&str]| parse_options(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(
            o(&["--host", "build-03"]).unwrap(),
            Options {
                dest: "build-03".into(),
                token_file: None,
            }
        );
        assert_eq!(
            o(&["--host=build-03", "--oauth-token-file", "/home/u/t"])
                .unwrap()
                .token_file
                .as_deref(),
            Some("/home/u/t")
        );
        assert!(
            o(&["--host", "build-03", "--oauth-token-file", "~/t"])
                .unwrap_err()
                .to_string()
                .contains("absolute path")
        );
        assert!(
            o(&["--host", "build-03", "--tmux", "/x"])
                .unwrap_err()
                .to_string()
                .contains("unknown option")
        );
        assert!(
            o(&["--host"])
                .unwrap_err()
                .to_string()
                .contains("needs a value")
        );
        assert!(o(&[]).unwrap_err().to_string().contains("needs --host"));
    }

    #[test]
    fn the_launch_script_exports_the_login_path_and_the_requests_env() {
        let mut r = req("ws://marshal.mesh:6155");
        r.env
            .insert("XDG_CONFIG_HOME".into(), "/from/launcher".into());
        let t = Target::from_request(&r).unwrap();
        let s = launch_script(&t, &r);
        assert!(s.starts_with("name='marshal-launch-1'"), "{s}");
        let inner = &s[s.find("inner=").unwrap()..];
        assert!(
            inner.contains("XDG_CONFIG_HOME='\"'\"'/from/launcher"),
            "{inner}"
        );
        assert!(!inner.contains("CLAUDE_CODE_OAUTH_TOKEN"), "{inner}");
        assert!(s.contains("inner=\"export PATH=$(q \"$PATH\")"), "{s}");
        let c = check_script(&t, &r);
        assert!(c.contains("req_xdg='/from/launcher'"), "{c}");
    }

    #[test]
    fn single_quotes_survive() {
        assert_eq!(sq("it's"), r#"'it'"'"'s'"#);
    }

    #[test]
    fn handles_round_trip() {
        let h = Handle {
            dest: "u@build-03".into(),
            name: "marshal-launch-1".into(),
        };
        assert_eq!(h.to_string(), "u@build-03 marshal-launch-1");
        assert_eq!(parse_handle(&h.to_string()).unwrap(), h);
        assert!(parse_handle("a b c").is_err());
        assert!(parse_handle("nospace").is_err());
    }

    /// The facts a target wired through the marshal-shim plugin reports.
    fn good_claude_probe(path: &str) -> String {
        format!(
            "profile noise\n@@host build-03\n@@daemon-port ok\n@@hook-port ok\n@@cwd /home/u/work/app\n\
             @@tmux ok\n@@harness ok\n@@shim marshal-shim {VERSION}\n\
             @@claude-plugin enabled\t{path}\n\
             @@claude-hook session-start\tmarshal-shim\t{path}/hooks/hooks.json\n\
             @@claude-hook wait\tmarshal-shim\t{path}/hooks/hooks.json\n\
             @@claude-mcp plugin\tplugin:marshal-shim:marshal\tmarshal-shim\t\t{path}/.mcp.json\n\
             @@done\n"
        )
    }

    #[test]
    fn a_wired_claude_target_passes() {
        let r = req("ws://marshal.mesh:6155");
        let t = Target::from_request(&r).unwrap();
        let p = Probe::parse(&good_claude_probe("/p/marshal-shim/0.60.2"));
        assert_eq!(p.failures(&t, &r), Vec::<String>::new());
    }

    fn one_failure(probe: &str, r: &LaunchRequest) -> String {
        let t = Target::from_request(r).unwrap();
        let f = Probe::parse(probe).failures(&t, r);
        assert_eq!(f.len(), 1, "{f:#?}");
        f[0].clone()
    }

    /// Wired by hand as upstream's README documents: an MCP server in
    /// ~/.claude.json and hooks in ~/.claude/settings.json, the shim by path.
    fn hand_wired_claude_probe(verbs: &[&str]) -> String {
        let mut s = format!(
            "@@host build-03\n@@daemon-port ok\n@@hook-port ok\n@@cwd /home/u/work/app\n\
             @@tmux ok\n@@harness ok\n@@shim marshal-shim {VERSION}\n"
        );
        for v in verbs {
            s.push_str(&format!(
                "@@claude-hook {v}\t/home/u/.local/bin/marshal-shim\t/home/u/.claude/settings.json\n"
            ));
        }
        s.push_str(
            "@@claude-mcp user\tmarshal\t/home/u/.local/bin/marshal-shim\t\t~/.claude.json\n",
        );
        s.push_str(&format!(
            "@@shim-at /home/u/.local/bin/marshal-shim\t/home/u/.local/bin/marshal-shim\tmarshal-shim {VERSION}\n@@done\n"
        ));
        s
    }

    #[test]
    fn a_hand_wired_claude_target_passes_once_it_has_the_wait_hook() {
        let r = req("ws://marshal.mesh:6155");
        let t = Target::from_request(&r).unwrap();
        let p = Probe::parse(&hand_wired_claude_probe(&[
            "session-start",
            "prompt-submit",
            "wait",
        ]));
        assert_eq!(p.failures(&t, &r), Vec::<String>::new());
        // The README's hooks alone have no waiter.
        let e = one_failure(
            &hand_wired_claude_probe(&["session-start", "prompt-submit"]),
            &r,
        );
        assert!(e.contains("not `marshal-shim claude-hook wait`"), "{e}");
        let untrusted = hand_wired_claude_probe(&["wait"])
            .replace("@@done", "@@untrusted-shim ./marshal-shim\n@@done");
        assert!(
            one_failure(&untrusted, &r).contains("outside the trusted marshal-shim installation")
        );
    }

    #[test]
    fn a_disabled_or_missing_plugin_with_nothing_by_hand_says_so() {
        let r = req("ws://marshal.mesh:6155");
        let t = Target::from_request(&r).unwrap();
        let bare = format!(
            "@@host build-03\n@@daemon-port ok\n@@hook-port ok\n@@cwd /w\n@@tmux ok\n\
             @@harness ok\n@@shim marshal-shim {VERSION}\n"
        );
        let off = format!("{bare}@@claude-plugin disabled\t/p\n@@done\n");
        let f = Probe::parse(&off).failures(&t, &r);
        assert_eq!(f.len(), 2, "{f:#?}");
        assert!(
            f[0].contains("no MCP server that runs marshal-shim"),
            "{}",
            f[0]
        );
        assert!(
            f[1].contains("don't run `marshal-shim claude-hook`"),
            "{}",
            f[1]
        );
        assert!(
            f.iter().all(|x| x.contains("installed but not enabled")),
            "{f:#?}"
        );
        let none = format!("{bare}@@done\n");
        assert_eq!(Probe::parse(&none).failures(&t, &r).len(), 2);
        let nojson = format!("{bare}@@no-json-reader\n@@done\n");
        let f = Probe::parse(&nojson).failures(&t, &r);
        assert!(
            f.iter().all(|x| x.contains("neither python3 nor jq")),
            "{f:#?}"
        );
    }

    #[test]
    fn each_failure_says_why() {
        let r = req("ws://marshal.mesh:6155");
        let good = good_claude_probe("/p/marshal-shim/0.60.2");
        let hooks = "@@claude-hook session-start\tmarshal-shim\t/p/marshal-shim/0.60.2/hooks/hooks.json\n\
                     @@claude-hook wait\tmarshal-shim\t/p/marshal-shim/0.60.2/hooks/hooks.json\n";
        let cases = [
            (
                "@@daemon-port ok",
                "@@daemon-port fail",
                "can't reach the daemon at marshal.mesh:6155",
            ),
            (
                "@@hook-port ok",
                "@@hook-port fail",
                "hook listener at marshal.mesh:6156",
            ),
            (
                "@@cwd /home/u/work/app",
                "@@cwd-missing /home/u/work/app",
                "has no folder",
            ),
            ("@@tmux ok", "@@tmux missing", "no tmux"),
            ("@@harness ok", "@@harness missing", "no `claude`"),
            (
                hooks,
                "",
                "hooks on build-03 don't run `marshal-shim claude-hook`",
            ),
            (
                "@@claude-hook wait\t",
                "@@claude-hook prompt-submit\t",
                "not `marshal-shim claude-hook wait`",
            ),
            (
                "@@claude-mcp plugin\tplugin:marshal-shim:marshal\tmarshal-shim\t\t/p/marshal-shim/0.60.2/.mcp.json\n",
                "",
                "has no MCP server that runs marshal-shim",
            ),
            ("@@done\n", "", "stopped before it finished"),
        ];
        for (from, to, want) in cases {
            let f = one_failure(&good.replacen(from, to, 1), &r);
            assert!(f.contains(want), "{from} → {to}: {f}");
        }
        let old = good.replace(&format!("marshal-shim {VERSION}"), "marshal-shim 0.1.0");
        assert!(one_failure(&old, &r).contains("on build-03: the target's marshal-shim is 0.1.0"));
        let file = good.replace(
            "@@done\n",
            "@@daemon-address-file /home/u/.config/marshal/daemon-address\n@@daemon-address ws://old:6155\n@@done\n",
        );
        let e = one_failure(&file, &r);
        assert!(
            e.contains("/home/u/.config/marshal/daemon-address naming ws://old:6155"),
            "{e}"
        );
        let same = good.replace(
            "@@done\n",
            "@@daemon-address-file /x\n@@daemon-address ws://marshal.mesh:6155\n@@done\n",
        );
        assert_eq!(
            Probe::parse(&same).failures(&Target::from_request(&r).unwrap(), &r),
            Vec::<String>::new()
        );
        let garbage = good.replace(&format!("@@shim marshal-shim {VERSION}"), "@@shim oops");
        assert!(one_failure(&garbage, &r).contains("has no version in it"));
        let none = good.replace(&format!("@@shim marshal-shim {VERSION}"), "@@shim-missing");
        assert!(one_failure(&none, &r).contains("no marshal-shim on its login PATH"));
        let fixed = good.replace(
            "@@claude-mcp plugin\tplugin:marshal-shim:marshal\tmarshal-shim\t\t",
            "@@claude-mcp plugin\tplugin:marshal-shim:marshal\tmarshal-shim\tws://other:6155\t",
        );
        assert!(one_failure(&fixed, &r).contains("fixed to ws://other:6155"));
        let same_addr = good.replace(
            "@@claude-mcp plugin\tplugin:marshal-shim:marshal\tmarshal-shim\t\t",
            "@@claude-mcp plugin\tplugin:marshal-shim:marshal\tmarshal-shim\tws://marshal.mesh:6155\t",
        );
        assert_eq!(
            Probe::parse(&same_addr).failures(&Target::from_request(&r).unwrap(), &r),
            Vec::<String>::new()
        );
    }

    #[test]
    fn codex_wiring() {
        let mut r = req("ws://marshal.mesh:6155");
        r.harness = "codex".into();
        r.argv = vec!["codex".into()];
        let t = Target::from_request(&r).unwrap();
        let base = format!(
            "@@host b\n@@daemon-port ok\n@@hook-port ok\n@@cwd /w\n@@tmux ok\n@@harness ok\n\
             @@shim marshal-shim {VERSION}\n@@codex-managed yes\n@@codex-mcp yes\n\
             @@codex-hooks yes\n@@codex-daemon match\n@@codex-hook-address match\n@@done\n"
        );
        assert_eq!(Probe::parse(&base).failures(&t, &r), Vec::<String>::new());
        let other = base.replace("@@codex-daemon match", "@@codex-daemon mismatch");
        assert!(one_failure(&other, &r).contains("configured for a different daemon"));
        let nohooks = base.replace("@@codex-hooks yes", "@@codex-hooks no");
        assert!(one_failure(&nohooks, &r).contains("don't run `marshal-shim codex-hook`"));
        let missing = base.replace("@@codex-managed yes", "@@codex-managed no");
        assert!(one_failure(&missing, &r).contains("codex-setup"));
    }
}
