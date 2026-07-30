//! `marshal-shim codex-hook <session-start|session-end|prompt-submit|pre-tool-use|post-tool-use> [base-url]`
//! — the Codex hook bridge, as a shim subcommand.
//!
//! Codex invokes this from `[hooks]` in `~/.codex/config.toml`. It reads Codex's
//! hook JSON on stdin, forwards `session_id` + `cwd` to the marshal daemon's
//! `/hook/*` endpoint, and emits the daemon's `<marshal_session>` /
//! `<marshal_inbox>` block as `additionalContext` so Codex injects it into the
//! model's turn. SessionStart surfaces the identity block; UserPromptSubmit AND
//! the two tool-use events surface the inbox — the tool-use events let an
//! actively-working agent pick up peer messages between tool calls, the closest
//! Codex gets to live delivery (it has no server→model push).
//!
//! Living as a SUBCOMMAND of the shim binary (rather than a shell script) makes
//! the integration cross-platform: the same `marshal-shim` / `marshal-shim.exe`
//! Codex already runs as its MCP server is also its hook handler, on Windows,
//! macOS, and Linux, with NO bash / jq / curl dependency. It dispatches BEFORE
//! the tokio runtime (like `--check` / `statusline`): one small blocking HTTP
//! POST, no async, no WebSocket.
//!
//! Best-effort by design: any failure prints nothing and returns, so marshal
//! can never block or break a Codex turn.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

const HOOK_PORT: u16 = 6156;
const TIMEOUT: Duration = Duration::from_secs(5);

/// `ep` is `session-start` or `prompt-submit`; `base_override` is an optional
/// explicit `http://host:port` (the deploy role passes it so the hook doesn't
/// depend on env the Codex hook process may not inherit).
pub fn run(ep: &str, base_override: Option<&str>) {
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let v: serde_json::Value =
        serde_json::from_str(input.trim()).unwrap_or(serde_json::Value::Null);
    let sid = v.get("session_id").and_then(|s| s.as_str()).unwrap_or("");
    if sid.is_empty() {
        return; // no session → nothing to surface
    }
    let cwd = v.get("cwd").and_then(|s| s.as_str()).unwrap_or("");

    let base = resolve_base(base_override);
    let host = short_host();
    let op = operator();
    // Map the hook entry point to (daemon endpoint, Codex hook-event name). The
    // tool-use entry points reuse the prompt-submit INBOX endpoint so an
    // actively-working agent surfaces peer messages at tool boundaries — not only
    // on a user prompt (Codex has no server→model push; hooks are the only inbound).
    let (endpoint, event) = match ep {
        "session-start" => ("session-start", "SessionStart"),
        "session-end" => ("session-end", "SessionEnd"),
        "prompt-submit" => ("prompt-submit", "UserPromptSubmit"),
        "pre-tool-use" => ("prompt-submit", "PreToolUse"),
        "post-tool-use" => ("prompt-submit", "PostToolUse"),
        _ => ("prompt-submit", "UserPromptSubmit"),
    };
    let body = format!(
        "{{\"session_id\":{},\"cwd\":{}}}",
        json_str(sid),
        json_str(cwd)
    );
    let path = format!(
        "/hook/{endpoint}?host={}&operator={}&harness=codex",
        url_q(&host),
        url_q(&op)
    );

    let Some(resp) = http_post(&base, &path, &body) else {
        return;
    };
    let resp = resp.trim_end_matches(['\n', '\r', ' ']);
    if resp.is_empty() {
        return; // empty inbox / no session block → inject nothing
    }
    let out = serde_json::json!({
        "hookSpecificOutput": { "hookEventName": event, "additionalContext": resp }
    });
    println!("{out}");
}

/// Register a Codex app-server thread without asking the daemon to surface
/// context. The long-lived bridge calls this from `thread/started`, before the
/// first prompt; normal lifecycle hooks remain responsible for context
/// injection and inbox acknowledgement.
pub(crate) fn register_session(base: &str, session_id: &str, cwd: &str) -> bool {
    let body = format!(
        "{{\"session_id\":{},\"cwd\":{}}}",
        json_str(session_id),
        json_str(cwd)
    );
    let path = format!(
        "/hook/session-register?host={}&operator={}&harness=codex",
        url_q(&short_host()),
        url_q(&operator())
    );
    http_post(base, &path, &body).is_some()
}

/// Resolve the hook listener used by a bridge that already resolved its daemon
/// WebSocket address. `MARSHAL_BASE_URL` remains the explicit override for
/// non-standard hook ports.
pub(crate) fn registration_base(daemon: &str) -> String {
    if let Ok(base) = std::env::var("MARSHAL_BASE_URL")
        && !base.is_empty()
    {
        return base.trim_end_matches('/').to_string();
    }
    base_from_daemon(daemon)
}

/// Resolve the daemon hook base URL: explicit arg → `MARSHAL_BASE_URL` env →
/// derived from the shim's daemon-address (`ws://host:6155` → `http://host:6156`)
/// → localhost default.
fn resolve_base(base_override: Option<&str>) -> String {
    if let Some(b) = base_override.filter(|b| !b.is_empty()) {
        return b.trim_end_matches('/').to_string();
    }
    if let Ok(b) = std::env::var("MARSHAL_BASE_URL")
        && !b.is_empty()
    {
        return b.trim_end_matches('/').to_string();
    }
    // Fall back to the same daemon address the shim's MCP path uses, remapped
    // from the WS endpoint to the plain-HTTP hook port.
    let ws = std::env::var(crate::ADDRESS_ENV)
        .ok()
        .or_else(crate::read_address_from_config_file)
        .unwrap_or_default();
    base_from_daemon(&ws)
}

fn base_from_daemon(daemon: &str) -> String {
    let authority = daemon
        .strip_prefix("ws://")
        .or_else(|| daemon.strip_prefix("wss://"))
        .unwrap_or("")
        .split('/')
        .next()
        .unwrap_or("");
    let host = if let Some(rest) = authority.strip_prefix('[') {
        rest.find(']')
            .map(|end| &authority[..=end + 1])
            .unwrap_or(authority)
    } else {
        authority
            .rsplit_once(':')
            .filter(|(_, port)| port.parse::<u16>().is_ok())
            .map(|(host, _)| host)
            .unwrap_or(authority)
    };
    let host = if host.is_empty() { "localhost" } else { host };
    format!("http://{host}:{HOOK_PORT}")
}

/// Minimal blocking HTTP/1.1 POST. The daemon replies `Content-Length` +
/// `Connection: close`, so we read to EOF and return the body. `None` on any
/// transport failure.
fn http_post(base: &str, path: &str, body: &str) -> Option<String> {
    let hostport = base.strip_prefix("http://")?.trim_end_matches('/');
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().ok()?),
        None => (hostport, 80),
    };
    let sock = format!("{host}:{port}").to_socket_addrs().ok()?.next()?;
    let mut stream = TcpStream::connect_timeout(&sock, TIMEOUT).ok()?;
    stream.set_read_timeout(Some(TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(TIMEOUT)).ok()?;
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).ok()?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    let idx = text.find("\r\n\r\n")?;
    let status = text[..idx].lines().next()?.split_whitespace().nth(1)?;
    if !status.starts_with('2') {
        return None;
    }
    Some(text[idx + 4..].to_string())
}

fn short_host() -> String {
    std::env::var("MARSHAL_HOST")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            gethostname::gethostname()
                .into_string()
                .ok()
                .and_then(|h| h.split('.').next().map(str::to_string))
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "codex".to_string())
}

fn operator() -> String {
    std::env::var("MARSHAL_OPERATOR")
        .or_else(|_| std::env::var("USER"))
        .or_else(|_| std::env::var("USERNAME"))
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "codex".to_string())
}

/// JSON-encode a string (quoted, escaped) for the request body.
fn json_str(s: &str) -> String {
    serde_json::Value::String(s.to_string()).to_string()
}

/// Minimal percent-encoding for a query-parameter value (host / operator).
/// Emails (`@`) and hostnames are safe; we only need to neutralise the few
/// characters that would break the query string.
fn url_q(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'@' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_hook_base_from_daemon_authority() {
        assert_eq!(
            base_from_daemon("ws://marshal.example:6155"),
            "http://marshal.example:6156"
        );
        assert_eq!(
            base_from_daemon("wss://marshal.example:443/myko/mcp"),
            "http://marshal.example:6156"
        );
        assert_eq!(base_from_daemon("ws://[::1]:6155"), "http://[::1]:6156");
        assert_eq!(base_from_daemon(""), "http://localhost:6156");
    }
}
