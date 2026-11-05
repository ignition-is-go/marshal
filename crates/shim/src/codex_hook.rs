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
//! `marshal-shim claude-hook <session-start|prompt-submit|post-tool-use>` runs
//! the same bridge for Claude Code (see [`Harness`]), so Claude sessions get the
//! inbox without the live-channel launch flag.
//!
//! Best-effort by design: any failure prints nothing and returns, so marshal
//! can never block or break a Codex or Claude turn.

use std::io::{Read, Write};
use std::time::Duration;

use crate::link::{PostFailure, Reply};

const HOOK_PORT: u16 = 6156;
const TIMEOUT: Duration = Duration::from_secs(5);

/// Which agent harness invoked the hook. Both send the same hook JSON and read
/// the same `hookSpecificOutput.additionalContext`; they differ in the
/// `harness=` the daemon sees (it words the `<marshal_session>` block per
/// harness) and in how long a request may take.
#[derive(Clone, Copy)]
pub enum Harness {
    Codex,
    /// Claude Code, via `marshal-shim claude-hook`. Its `PostToolUse` hook runs
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
    // `ack=client`: the daemon marks the mail read only when this hook says it
    // has printed it, so a reply lost on the way (a hook killed at its timeout,
    // a dropped proxy hop) leaves the mail unread.
    let path = format!(
        "/hook/{endpoint}?host={}&operator={}&harness={}&ack=client",
        url_q(&host),
        url_q(&op),
        harness.name()
    );

    let Ok(reply) = http_post_why(&base, &path, &body, harness.timeout()) else {
        return;
    };
    let resp = reply.body.trim_end_matches(['\n', '\r', ' ']);
    if !resp.is_empty() {
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
        let _ = http_post_why(&base, "/hook/ack", &ack, ACK_TIMEOUT);
    }
}

/// `marshal-shim watch <session_id> [base-url]` — the live inbox for Claude
/// Code without the channels flag. Run under Claude's Monitor tool: it
/// long-polls the daemon's `/hook/wait` and prints each `<marshal_inbox>` block
/// it gets, and every line it prints becomes a Monitor event, which wakes an
/// idle session. Runs until killed (the Monitor's expiry); a daemon that can't
/// be reached is retried, never fatal.
///
/// With `once` it exits after the first delivery instead, for running as a
/// background Bash command: that has no 30-minute limit and costs no model
/// turns while idle, and its exit is what wakes the session. Nothing bounds its
/// lifetime, so it also exits between polls once its parent is gone, rather
/// than taking mail for a session that has closed.
pub fn watch(session_id: &str, base_override: Option<&str>, once: bool) {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    wait_for_mail(session_id, &cwd, base_override, once, |resp| {
        let wrapped = wrap_long_lines(resp, WATCH_LINE_CHARS);
        for (i, chunk) in event_chunks(&wrapped, WATCH_EVENT_CHARS).iter().enumerate() {
            if i > 0 {
                std::thread::sleep(WATCH_EVENT_GAP);
            }
            let mut out = std::io::stdout().lock();
            let _ = writeln!(out, "{chunk}");
            let _ = out.flush();
        }
    });
}

/// `marshal-shim claude-hook wait` — the flag-free wake for Claude Code, run as
/// an `asyncRewake` hook on SessionStart and Stop. It holds a `/hook/wait`
/// long-poll and, when mail arrives, prints it to stderr and exits 2, which
/// starts a turn even in a session nobody has typed to. Stop runs it again, so
/// no agent has to keep anything armed. Stop fires every turn, so a per-session
/// lock keeps it to one waiter; the lock is released whenever the process ends.
pub fn wait_hook(base_override: Option<&str>) {
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let v: serde_json::Value =
        serde_json::from_str(input.trim()).unwrap_or(serde_json::Value::Null);
    let sid = v.get("session_id").and_then(|s| s.as_str()).unwrap_or("");
    if sid.is_empty() {
        return;
    }
    let cwd = v.get("cwd").and_then(|s| s.as_str()).unwrap_or("");
    // A resumed session's old waiter holds the lock until its next poll notices
    // the parent is gone, so wait that out rather than leave the session without
    // a waiter. If that waiter's parent is alive, this just exits a bit later.
    let deadline = std::time::Instant::now() + lock_wait();
    let _lock = loop {
        match waiter_lock(sid) {
            Some(lock) => break lock,
            None if std::time::Instant::now() >= deadline => return,
            None => std::thread::sleep(LOCK_RETRY),
        }
    };
    let delivered = wait_for_mail(sid, cwd, base_override, true, |resp| {
        eprintln!("{resp}");
    });
    if delivered {
        std::process::exit(2);
    }
}

/// Take this session's waiter lock, or `None` if another waiter holds it.
fn waiter_lock(session_id: &str) -> Option<std::fs::File> {
    let name = format!("waiter-{session_id}");
    let path = crate::config_file_candidates(&name).into_iter().next()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok()?;
    }
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .ok()?;
    file.try_lock().ok()?;
    Some(file)
}

/// Long-poll `/hook/wait`, handing each delivery to `deliver` and then acking it.
/// Returns true after the first delivery when `once`, and false if the parent
/// went away first. Without `once` it runs until killed or orphaned.
fn wait_for_mail(
    session_id: &str,
    cwd: &str,
    base_override: Option<&str>,
    once: bool,
    deliver: impl Fn(&str),
) -> bool {
    // A one-shot waiter only notices its parent is gone between polls, and until
    // then it could take mail for a closed session, so it polls more often.
    let wait = if once { ONCE_WAIT } else { WATCH_WAIT };
    let body = format!(
        "{{\"session_id\":{},\"cwd\":{}}}",
        json_str(session_id),
        json_str(cwd)
    );
    let path = format!(
        "/hook/wait?host={}&operator={}&harness=claude&ack=client&timeout={}",
        url_q(&short_host()),
        url_q(&operator()),
        wait.as_secs()
    );
    let base = resolve_base(base_override);
    // The daemon answers within `WATCH_WAIT`; the read timeout only has to
    // outlast it, so a daemon that vanished mid-wait is noticed.
    let timeout = wait + Duration::from_secs(30);
    // Report only a change of cause: stdout would wake the session, and
    // repeating it every retry would just bury the Monitor's output file.
    let mut failing = None;
    // After the daemon says it's stopping, stay quiet about it being unreachable
    // until this deadline, since that's expected while it restarts.
    let mut restart_quiet_until: Option<std::time::Instant> = None;
    let parent = parent_pid();
    loop {
        if parent_pid() != parent {
            return false;
        }
        let post = |path: &str, body: &str, timeout| http_post_why(&base, path, body, timeout);
        match post(&path, &body, timeout) {
            Ok(reply) => {
                failing = None;
                clear_refusal(session_id);
                if reply.restarting {
                    restart_quiet_until = Some(std::time::Instant::now() + RESTART_QUIET);
                    std::thread::sleep(RESTART_BACKOFF);
                    continue;
                }
                let resp = reply.body.trim_end_matches(['\n', '\r', ' ']);
                if !resp.is_empty() {
                    // The session closed during the poll: leave the mail unacked,
                    // for whoever reads next once the daemon's lease lapses.
                    if parent_pid() != parent {
                        return false;
                    }
                    deliver(resp);
                    // Only now has it been handed on. A lost ack means the mail
                    // comes again, which beats losing it.
                    if !reply.ack_ids.is_empty() {
                        let ack = serde_json::json!({
                            "session_id": session_id,
                            "message_ids": reply.ack_ids,
                        })
                        .to_string();
                        if let Err(e) = post("/hook/ack", &ack, ACK_TIMEOUT) {
                            eprintln!(
                                "marshal-shim watch: couldn't ack delivered mail, so it may arrive again: {e}"
                            );
                        }
                    }
                    if once {
                        return true;
                    }
                }
            }
            Err(failure) => {
                // A refusal won't clear by itself, so tell the session once per
                // cause, on the path it reads: Monitor ignores a watcher's stderr.
                // A new waiter or re-armed watcher mustn't tell it again, or
                // every Stop would wake it.
                if failure.is_refusal() && refusal_is_new(session_id, failure) {
                    deliver(&format!(
                        "Marshal can't deliver mail to this session: {failure}. Please tell your operator. This notice won't repeat."
                    ));
                    if once {
                        return true;
                    }
                }
                // A one-shot waiter stops polling on a refusal. Its stderr reaches
                // the model with its next message, by which time an outage would
                // be long out of date, so it stays quiet about those.
                if once && failure.is_refusal() {
                    return false;
                }
                let restarting =
                    restart_quiet_until.is_some_and(|until| std::time::Instant::now() < until);
                if failing != Some(failure) && !once && !restarting {
                    eprintln!("marshal-shim watch: {failure}; retrying");
                    failing = Some(failure);
                }
                std::thread::sleep(WATCH_RETRY);
            }
        }
    }
}

fn refusal_marker(session_id: &str) -> Option<std::path::PathBuf> {
    crate::config_file_candidates(&format!("refused-{session_id}"))
        .into_iter()
        .next()
}

/// Whether this session hasn't yet been told of this cause, recording that it
/// now has. If the marker can't be written, it says no: staying quiet beats
/// waking the session at every Stop.
fn refusal_is_new(session_id: &str, failure: PostFailure) -> bool {
    let Some(path) = refusal_marker(session_id) else {
        return false;
    };
    let cause = failure.to_string();
    if std::fs::read_to_string(&path).is_ok_and(|seen| seen == cause) {
        return false;
    }
    if let Some(dir) = path.parent()
        && std::fs::create_dir_all(dir).is_err()
    {
        return false;
    }
    std::fs::write(&path, cause).is_ok()
}

fn clear_refusal(session_id: &str) {
    if let Some(path) = refusal_marker(session_id) {
        let _ = std::fs::remove_file(path);
    }
}

/// This process's parent, which changes when the parent exits. `None` where
/// that can't be checked, so `watch --once` simply keeps running there.
fn parent_pid() -> Option<u32> {
    #[cfg(unix)]
    {
        Some(std::os::unix::process::parent_id())
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// How long one `watch` long-poll asks the daemon to hold, and how long to
/// back off when the daemon can't be reached.
const WATCH_WAIT: Duration = Duration::from_secs(240);
const ONCE_WAIT: Duration = Duration::from_secs(30);
const WATCH_RETRY: Duration = Duration::from_secs(5);
/// How long to pause after the daemon ends a wait because it's stopping, and
/// how long its being unreachable goes unreported afterwards.
const RESTART_BACKOFF: Duration = Duration::from_secs(2);
const RESTART_QUIET: Duration = Duration::from_secs(30);
const ACK_TIMEOUT: Duration = Duration::from_secs(10);
const LOCK_RETRY: Duration = Duration::from_secs(1);

/// How long a waiter waits for another to let go: past one poll of `ONCE_WAIT`.
fn lock_wait() -> Duration {
    std::env::var("MARSHAL_WAITER_LOCK_WAIT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map_or(ONCE_WAIT + Duration::from_secs(5), Duration::from_secs)
}
/// Longest line `watch` prints. Monitor cuts an event's line at about 500
/// characters, so a longer one would reach the session with its end missing.
const WATCH_LINE_CHARS: usize = 400;
/// Longest single Monitor event. Monitor also cuts a whole event at about 3,000
/// characters, and joins lines printed within 200ms into one event, so a large
/// batch goes out in pieces with a pause between them.
const WATCH_EVENT_CHARS: usize = 2_500;
const WATCH_EVENT_GAP: Duration = Duration::from_millis(400);

/// Group whole lines into chunks of at most `max` characters. A single longer
/// line gets its own chunk; `wrap_long_lines` keeps that from happening here.
fn event_chunks(text: &str, max: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_chars = 0;
    for line in text.split('\n') {
        let chars = line.chars().count();
        if current_chars > 0 && current_chars + 1 + chars > max {
            chunks.push(std::mem::take(&mut current));
            current_chars = 0;
        }
        if current_chars > 0 {
            current.push('\n');
            current_chars += 1;
        }
        current.push_str(line);
        current_chars += chars;
    }
    chunks.push(current);
    chunks
}

/// Break lines longer than `max` characters, at a space when there is one.
fn wrap_long_lines(text: &str, max: usize) -> String {
    let mut out = String::with_capacity(text.len());
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let mut rest = line;
        while let Some((limit, _)) = rest.char_indices().nth(max) {
            // A space at the very start would leave an empty line behind.
            let cut = match rest[..limit].rfind(' ') {
                Some(space) if space > 0 => space,
                _ => limit,
            };
            out.push_str(&rest[..cut]);
            out.push('\n');
            rest = rest[cut..].strip_prefix(' ').unwrap_or(&rest[cut..]);
        }
        out.push_str(rest);
    }
    out
}

/// Register a Codex app-server thread without asking the daemon to surface
/// context. The long-lived bridge calls this from `thread/started`, before the
/// first prompt; normal lifecycle hooks remain responsible for context
/// injection and inbox acknowledgement.
pub(crate) fn register_session(base: &str, session_id: &str, cwd: &str) -> Option<String> {
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
    let response = http_post(base, &path, &body, TIMEOUT)?;
    if let Ok(identity) = serde_json::from_str::<serde_json::Value>(&response)
        && identity.get("session_id").and_then(|v| v.as_str()) == Some(session_id)
        && let Some(nickname) = identity
            .get("nickname")
            .and_then(|v| v.as_str())
            .filter(|nickname| !nickname.is_empty())
    {
        crate::write_assigned_nickname(session_id, nickname);
        return Some(nickname.to_string());
    }
    None
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
/// transport failure. `timeout` bounds the connect and each read/write.
fn http_post(base: &str, path: &str, body: &str, timeout: Duration) -> Option<String> {
    http_post_why(base, path, body, timeout)
        .ok()
        .map(|reply| reply.body)
}

/// [`http_post`], saying why it failed.
fn http_post_why(
    base: &str,
    path: &str,
    body: &str,
    timeout: Duration,
) -> Result<Reply, PostFailure> {
    let unreachable = |_| PostFailure::Unreachable;
    let hostport = base
        .strip_prefix("http://")
        .ok_or(PostFailure::Unreachable)?
        .trim_end_matches('/');
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().map_err(|_| PostFailure::Unreachable)?),
        None => (hostport, 80),
    };
    let mut stream = crate::link::connect_any(format!("{host}:{port}"), timeout)
        .ok_or(PostFailure::Unreachable)?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(unreachable)?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(unreachable)?;
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: {hostport}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).map_err(unreachable)?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).map_err(unreachable)?;
    crate::link::parse_response(&buf)
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
    fn event_chunks_keep_lines_whole_and_under_the_limit() {
        assert_eq!(
            event_chunks("aaaa\nbbbb\ncc\ndddddd", 9),
            ["aaaa\nbbbb", "cc\ndddddd"]
        );
    }

    #[test]
    fn event_chunks_lose_nothing() {
        let text = "one\n\ntwo\nthree";
        assert_eq!(event_chunks(text, 5).join("\n"), text);
        assert_eq!(event_chunks("", 5), [""]);
    }

    #[test]
    fn wrap_long_lines_keeps_every_word_under_the_limit() {
        let text = "one two three four\nshort";
        let wrapped = wrap_long_lines(text, 9);
        assert_eq!(wrapped, "one two\nthree\nfour\nshort");
    }

    #[test]
    fn wrap_long_lines_splits_a_long_word_on_a_char_boundary() {
        let wrapped = wrap_long_lines("ééééé", 2);
        assert_eq!(wrapped, "éé\néé\né");
    }

    #[test]
    fn wrap_long_lines_leaves_short_text_alone() {
        let text = "<marshal_inbox count=\"1\">\n- from a: hi\n</marshal_inbox>";
        assert_eq!(wrap_long_lines(text, 400), text);
    }

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
