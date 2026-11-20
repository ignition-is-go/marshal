//! `marshal-stdio codex-hook <session-start|session-end|prompt-submit|pre-tool-use|post-tool-use> [base-url]`
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
//! the integration cross-platform: the same `marshal-stdio` / `marshal-stdio.exe`
//! Codex already runs as its MCP server is also its hook handler, on Windows,
//! macOS, and Linux, with NO bash / jq / curl dependency. It dispatches BEFORE
//! the tokio runtime (like `--check` / `statusline`): one small blocking HTTP
//! POST, no async, no WebSocket.
//!
//! `marshal-stdio claude-hook <session-start|prompt-submit|post-tool-use>` runs
//! the same bridge for Claude Code (see [`Harness`]), so Claude sessions get the
//! inbox without the live-channel launch flag.
//!
//! Best-effort by design: any failure prints nothing and returns, so marshal
//! can never block or break a Codex or Claude turn.

use std::io::{Read, Write};
use std::net::{SocketAddr, ToSocketAddrs};
use std::time::{Duration, Instant};

use crate::hook_http::{PostFailure, Reply};

const HOOK_PORT: u16 = 6156;
const TIMEOUT: Duration = Duration::from_secs(3);

/// Which agent harness invoked the hook. Both send the same hook JSON and read
/// the same `hookSpecificOutput.additionalContext`; they differ in the
/// `harness=` the daemon sees (it words the `<marshal_session>` block per
/// harness) and in how long a request may take.
#[derive(Clone, Copy)]
pub enum Harness {
    Codex,
    /// Claude Code, via `marshal-stdio claude-hook`. Its `PostToolUse` hook runs
    /// after every tool call, so a slow daemon must not stall each one for the
    /// Codex budget.
    Claude,
}

impl Harness {
    fn name(self) -> &'static str {
        match self {
            Harness::Codex => "codex",
            Harness::Claude => "claude",
        }
    }

    fn timeout(self) -> Duration {
        match self {
            Harness::Codex => TIMEOUT,
            Harness::Claude => Duration::from_secs(2),
        }
    }
}

/// `ep` is `session-start` or `prompt-submit`; `base_override` is an optional
/// explicit `http://host:port` (the deploy role passes it so the hook doesn't
/// depend on env the Codex hook process may not inherit).
pub fn run(ep: &str, base_override: Option<&str>) {
    run_for(Harness::Codex, ep, base_override);
}

/// Shared hook body for both harnesses.
pub fn run_for(harness: Harness, ep: &str, base_override: Option<&str>) {
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let v: serde_json::Value =
        serde_json::from_str(input.trim()).unwrap_or(serde_json::Value::Null);
    // Codex uses the root session ID for spawned descendants. Their thread
    // ID is in subagent.agent_id, the same identity used by MCP tool calls.
    let id = match (harness, v.get("subagent")) {
        (Harness::Codex, Some(subagent)) if !subagent.is_null() => subagent.get("agent_id"),
        _ => v.get("session_id"),
    };
    let Some(sid) = id
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.trim().is_empty())
    else {
        return;
    };
    let override_address = base_override
        .filter(|address| !address.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            std::env::var("MARSHAL_BASE_URL")
                .ok()
                .filter(|address| !address.is_empty())
        });
    let myko_address = || {
        override_address
            .as_deref()
            .filter(|address| address.starts_with("ws://") || address.starts_with("wss://"))
            .map_or_else(crate::config::server_address, str::to_owned)
    };
    if ep == "session-end" {
        end_session(sid, myko_address(), harness.timeout());
        return;
    }
    let cwd = v
        .get("cwd")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            v.pointer("/workspace/current_dir")
                .and_then(serde_json::Value::as_str)
        })
        .unwrap_or("");
    let legacy_base = override_address
        .as_deref()
        .filter(|address| address.starts_with("http://"));
    let Some(base) = legacy_base else {
        run_myko_hook(
            harness,
            ep,
            &myko_address(),
            sid,
            cwd,
            v.get("subagent").is_none_or(serde_json::Value::is_null),
        );
        return;
    };
    let base = base.trim_end_matches('/');
    let host = short_host();
    let op = operator();
    // Map the hook entry point to (daemon endpoint, Codex hook-event name). The
    // tool-use entry points reuse the prompt-submit INBOX endpoint so an
    // actively-working agent surfaces peer messages at tool boundaries — not only
    // on a user prompt (Codex has no server→model push; hooks are the only inbound).
    let (endpoint, event) = match ep {
        "session-start" => ("session-start", "SessionStart"),
        "prompt-submit" => ("prompt-submit", "UserPromptSubmit"),
        "pre-tool-use" => ("prompt-submit", "PreToolUse"),
        "post-tool-use" => ("prompt-submit", "PostToolUse"),
        _ => ("prompt-submit", "UserPromptSubmit"),
    };
    let body = registration_body(sid, cwd, agent_id_from_env().as_deref());
    // `ack=client`: the daemon marks the mail read only when this hook says it
    // has printed it, so a reply lost on the way (a hook killed at its timeout,
    // a dropped proxy hop) leaves the mail unread.
    let path = format!(
        "/hook/{endpoint}?host={}&operator={}&harness={}&ack=client",
        url_q(&host),
        url_q(&op),
        harness.name()
    );

    let deadline = Instant::now() + harness.timeout();
    let Ok(reply) = http_post_why(base, &path, &body, deadline) else {
        return;
    };
    let resp = reply.body.trim_end_matches(['\n', '\r', ' ']);
    if resp.is_empty() {
        return;
    }
    if ep == "session-start"
        && v.get("subagent").is_none_or(serde_json::Value::is_null)
        && resp.contains("<marshal_session ")
    {
        record_launch_session(sid);
    }
    {
        let out = serde_json::json!({
            "hookSpecificOutput": { "hookEventName": event, "additionalContext": resp }
        });
        let mut stdout = std::io::stdout().lock();
        if writeln!(stdout, "{out}")
            .and_then(|()| stdout.flush())
            .is_err()
        {
            return; // not handed on: leave it unacked, to be offered again
        }
    }
    // Only now has the mail been handed on. A lost ack means it comes again
    // once the lease lapses, which beats losing it.
    if !reply.ack_ids.is_empty() {
        let ack =
            serde_json::json!({ "session_id": sid, "message_ids": reply.ack_ids }).to_string();
        let _ = http_post_why(base, "/hook/ack", &ack, deadline);
    }
}

fn run_myko_hook(harness: Harness, ep: &str, address: &str, sid: &str, cwd: &str, root: bool) {
    use hyphae::Gettable as _;
    use marshal_entities::{
        AckMessages, AckMessagesResult, HostInfo, RegisterHarnessSession, SessionId,
    };

    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return;
    };
    runtime.block_on(async {
        let client = crate::MarshalClient::new();
        client.set_address(Some(address.to_owned()));
        let _ = tokio::time::timeout(harness.timeout(), async {
            let session_id = SessionId(std::sync::Arc::from(sid));
            let registered = client.send_command::<RegisterHarnessSession, String>(&RegisterHarnessSession {
                session_id: session_id.clone(),
                cwd: cwd.into(),
                repository: (ep == "session-start").then(|| crate::config::repository_key(cwd)).flatten(),
                agent_id: root.then(agent_id_from_env).flatten(),
                operator: Some(operator()),
                host: Some(HostInfo {
                    name: short_host(),
                    os: std::env::consts::OS.into(),
                    arch: std::env::consts::ARCH.into(),
                }),
                activity: None,
                waiting: false,
            });
            let nickname = loop {
                if let Some(result) = registered.get() {
                    let Ok(nickname) = result else { return };
                    break nickname;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            };
            crate::config::write_assigned_nickname(sid, &nickname);
            if ep == "session-start" && root {
                record_launch_session(sid);
            }
            let Some(inbox) = request_harness_inbox(
                &client, sid, matches!(harness, Harness::Codex), ep == "session-start"
            ).await else { return };
            if inbox.context.is_empty() {
                return;
            }
            let event = match ep {
                "session-start" => "SessionStart",
                "pre-tool-use" => "PreToolUse",
                "post-tool-use" => "PostToolUse",
                _ => "UserPromptSubmit",
            };
            {
                let mut stdout = std::io::stdout().lock();
                let output = serde_json::json!({
                    "hookSpecificOutput": {"hookEventName": event, "additionalContext": inbox.context}
                });
                if writeln!(stdout, "{output}").and_then(|()| stdout.flush()).is_err() {
                    return;
                }
            }
            if !inbox.message_ids.is_empty() {
                let acked = client.send_command::<AckMessages, AckMessagesResult>(&AckMessages {
                    message_ids: inbox.message_ids,
                    as_session: Some(session_id),
                });
                while acked.get().is_none() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        })
        .await;
    });
}

pub async fn request_harness_inbox(
    client: &crate::MarshalClient,
    session_id: &str,
    codex: bool,
    session_start: bool,
) -> Option<marshal_entities::HarnessInbox> {
    use hyphae::Gettable as _;
    let response = client.send_command_raw_result(myko::wire::WrappedCommand {
        command_id: "SurfaceHarnessInbox".into(),
        command: serde_json::json!({
            "tx": uuid::Uuid::new_v4().to_string(),
            "sessionId": session_id,
            "codex": codex,
            "sessionStart": session_start,
        }),
    });
    loop {
        if let Some(result) = response.get() {
            return result
                .ok()
                .and_then(|value| serde_json::from_value(value).ok());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn end_session(session: &str, address: String, budget: Duration) {
    use hyphae::Gettable as _;
    use marshal_entities::{EndHarnessSession, SessionId};
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return;
    };
    runtime.block_on(async {
        let client = crate::MarshalClient::new();
        client.set_address(Some(address));
        let _ = tokio::time::timeout(budget, async {
            while !matches!(
                client.connection_status().get(),
                crate::ConnectionStatus::Connected(_)
            ) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let response = client.send_command::<EndHarnessSession, ()>(&EndHarnessSession {
                session_id: SessionId(std::sync::Arc::from(session)),
            });
            while response.get().is_none() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
    });
}

/// Record the first registered harness session for a dispatched launch.
pub fn record_launch_session(session_id: &str) {
    let Some(path) = std::env::var_os("MARSHAL_HOST_JOIN_FILE") else {
        return;
    };
    let Ok(mut file) = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    else {
        return;
    };
    // The join file is in a private directory allocated by the launch host.
    // The first session wins, including when a spawned child shares the env.
    let _ = serde_json::to_writer(&mut file, session_id);
}

/// Resolve the daemon hook base URL: explicit arg → `MARSHAL_BASE_URL` env →
/// derived from the shim's daemon-address (`ws://host:6155` → `http://host:6156`)
/// → localhost default.
pub fn resolve_base(base_override: Option<&str>) -> String {
    if let Some(b) = base_override.filter(|b| !b.is_empty()) {
        return b.trim_end_matches('/').to_string();
    }
    if let Ok(b) = std::env::var("MARSHAL_BASE_URL")
        && !b.is_empty()
    {
        return b.trim_end_matches('/').to_string();
    }
    base_from_daemon(&crate::config::server_address())
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

/// Blocking HTTP/1.1 POST with a complete Content-Length response.
/// One deadline covers resolution, connection, writes and framed reads.
pub fn http_post(base: &str, path: &str, body: &str, timeout: Duration) -> Option<String> {
    http_post_why(base, path, body, Instant::now() + timeout)
        .ok()
        .map(|reply| reply.body)
}

/// [`http_post`], saying why it failed.
pub fn http_post_why(
    base: &str,
    path: &str,
    body: &str,
    deadline: Instant,
) -> Result<Reply, PostFailure> {
    let left = || {
        deadline
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
            .ok_or(PostFailure::Unreachable)
    };
    let hostport = base
        .strip_prefix("http://")
        .ok_or(PostFailure::Unreachable)?
        .trim_end_matches('/');
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().map_err(|_| PostFailure::Unreachable)?),
        None => (hostport, 80),
    };
    let addrs =
        resolve_within(format!("{host}:{port}"), left()?).ok_or(PostFailure::Unreachable)?;
    let mut stream = crate::hook_http::connect_any(addrs.as_slice(), deadline)
        .ok_or(PostFailure::Unreachable)?;
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: {hostport}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut bytes = req.as_bytes();
    while !bytes.is_empty() {
        stream
            .set_write_timeout(Some(left()?))
            .map_err(|_| PostFailure::Unreachable)?;
        let written = stream.write(bytes).map_err(|_| PostFailure::Unreachable)?;
        if written == 0 {
            return Err(PostFailure::Unreachable);
        }
        bytes = &bytes[written..];
    }
    crate::hook_http::read_reply(&mut stream, deadline)
}

fn resolve_within(hostport: String, limit: Duration) -> Option<Vec<SocketAddr>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(hostport.to_socket_addrs().map(Iterator::collect));
    });
    rx.recv_timeout(limit).ok()?.ok()
}

pub fn short_host() -> String {
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

pub fn operator() -> String {
    std::env::var("MARSHAL_OPERATOR")
        .or_else(|_| std::env::var("USER"))
        .or_else(|_| std::env::var("USERNAME"))
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "codex".to_string())
}

/// Minimal percent-encoding for a query-parameter value (host / operator).
/// Emails (`@`) and hostnames are safe; we only need to neutralise the few
/// characters that would break the query string.
pub fn url_q(s: &str) -> String {
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

pub fn agent_id_from_env() -> Option<String> {
    std::env::var("MARSHAL_AGENT_ID")
        .or_else(|_| std::env::var("MARSHAL_LAUNCH_ID"))
        .ok()
        .and_then(|value| {
            let value = value.trim();
            (!value.is_empty()).then(|| value.to_owned())
        })
}

pub fn registration_body(session_id: &str, cwd: &str, agent_id: Option<&str>) -> String {
    registration_payload(session_id, cwd, agent_id).to_string()
}

fn registration_payload(session_id: &str, cwd: &str, agent_id: Option<&str>) -> serde_json::Value {
    let mut body = serde_json::json!({ "session_id": session_id, "cwd": cwd });
    if let Some(agent_id) = agent_id {
        body["agent_id"] = agent_id.into();
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_requests_preserve_the_non_default_port_in_host() {
        use std::io::{BufRead, BufReader};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let authority = listener.local_addr().unwrap().to_string();
        let expected = format!("Host: {authority}\r\n");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            let mut reader = BufReader::new(&mut stream);
            let mut headers = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                assert!(!line.is_empty(), "request ended before headers");
                headers.push_str(&line);
            }
            let mut body = [0; 2];
            reader.read_exact(&mut body).unwrap();
            assert_eq!(&body, b"{}");
            let status = if headers.contains(&expected) {
                "200 OK"
            } else {
                "400 Bad Request"
            };
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
            )
            .unwrap();
        });
        let response = http_post(
            &format!("http://{authority}"),
            "/hook/session-register",
            "{}",
            TIMEOUT,
        );
        server.join().unwrap();
        assert_eq!(
            response.as_deref(),
            Some("ok"),
            "strict HTTP authority validation rejected the hook request"
        );
    }

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
