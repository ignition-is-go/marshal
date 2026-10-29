//! Integration test: `marshal-shim claude-hook` — the Claude Code side of the
//! hook inbox. Drives the REAL binary against a fake `/hook/*` listener and
//! checks the request it sends and the `hookSpecificOutput` it prints, plus
//! that every hook the plugin ships answers for the event it is registered
//! under.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const SHIM: &str = env!("CARGO_BIN_EXE_marshal-shim");
const HOOK_INPUT: &str = r#"{"session_id":"itest-claude","cwd":"/tmp/proj"}"#;
const HOOKS_JSON: &str = include_str!("../../../plugins/marshal-shim/hooks/hooks.json");

/// What the fake listener saw: the request line and the body.
struct Seen {
    request_line: String,
    body: String,
}

/// Serve exactly one hook request, replying 200 with `reply`. Returns the base
/// URL to pass the shim and a receiver for what the request carried.
fn fake_hook_listener(reply: &'static str) -> (String, mpsc::Receiver<Seen>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut request_line = String::new();
        reader.read_line(&mut request_line).unwrap();
        let mut content_length = 0;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" || line.is_empty() {
                break;
            }
            if let Some((k, v)) = line.split_once(':')
                && k.eq_ignore_ascii_case("content-length")
            {
                content_length = v.trim().parse().unwrap();
            }
        }
        let mut body = vec![0; content_length];
        reader.read_exact(&mut body).unwrap();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
            reply.len()
        )
        .unwrap();
        let _ = tx.send(Seen {
            request_line: request_line.trim_end().to_string(),
            body: String::from_utf8(body).unwrap(),
        });
    });
    (base, rx)
}

/// Run `marshal-shim <args>` with the hook JSON on stdin; return stdout.
fn run_hook(args: &[&str], base: &str) -> String {
    let mut child = Command::new(SHIM)
        .args(args)
        .env("MARSHAL_BASE_URL", base)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn marshal-shim claude-hook");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(HOOK_INPUT.as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("wait");
    assert!(out.status.success(), "claude-hook must exit 0");
    String::from_utf8(out.stdout).unwrap()
}

fn additional_context(stdout: &str, event: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("hook stdout is not JSON ({e}): {stdout:?}"));
    let out = &v["hookSpecificOutput"];
    assert_eq!(out["hookEventName"], event, "wrong hookEventName: {v}");
    out["additionalContext"].as_str().unwrap().to_string()
}

#[test]
fn prompt_submit_pulls_the_inbox_as_claude() {
    let inbox = "<marshal_inbox count=\"1\">\n- from peer: hi\n</marshal_inbox>\n";
    let (base, seen) = fake_hook_listener(inbox);
    let stdout = run_hook(&["claude-hook", "prompt-submit"], &base);
    let seen = seen.recv_timeout(Duration::from_secs(5)).unwrap();

    assert!(
        seen.request_line.starts_with("POST /hook/prompt-submit?"),
        "{}",
        seen.request_line
    );
    assert!(
        seen.request_line.contains("harness=claude"),
        "{}",
        seen.request_line
    );
    let body: serde_json::Value = serde_json::from_str(&seen.body).unwrap();
    assert_eq!(body["session_id"], "itest-claude");
    assert_eq!(body["cwd"], "/tmp/proj");
    assert_eq!(
        additional_context(&stdout, "UserPromptSubmit"),
        inbox.trim_end()
    );
}

#[test]
fn post_tool_use_reads_the_inbox_endpoint() {
    let (base, seen) = fake_hook_listener("<marshal_inbox count=\"1\">\n</marshal_inbox>");
    let stdout = run_hook(&["claude-hook", "post-tool-use"], &base);
    let seen = seen.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(seen.request_line.starts_with("POST /hook/prompt-submit?"));
    additional_context(&stdout, "PostToolUse");
}

#[test]
fn empty_inbox_prints_nothing() {
    let (base, _seen) = fake_hook_listener("");
    let stdout = run_hook(&["claude-hook", "prompt-submit"], &base);
    assert!(
        stdout.is_empty(),
        "empty inbox must add no context: {stdout:?}"
    );
}

#[test]
fn unreachable_daemon_prints_nothing_and_returns_promptly() {
    let started = Instant::now();
    // Port 1 on loopback: nothing listens, connect is refused.
    let stdout = run_hook(&["claude-hook", "prompt-submit"], "http://127.0.0.1:1");
    assert!(stdout.is_empty(), "{stdout:?}");
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[test]
fn silent_daemon_cannot_stall_a_claude_turn() {
    // Accepts the connection and never answers: the Claude timeout must bound it.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let _hold = thread::spawn(move || {
        let conn = listener.accept();
        thread::sleep(Duration::from_secs(10));
        drop(conn);
    });
    let started = Instant::now();
    let stdout = run_hook(&["claude-hook", "prompt-submit"], &base);
    assert!(stdout.is_empty(), "{stdout:?}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "took {:?}",
        started.elapsed()
    );
}

#[test]
fn shipped_hooks_answer_for_the_event_they_are_registered_under() {
    let hooks: serde_json::Value = serde_json::from_str(HOOKS_JSON).unwrap();
    let events = hooks["hooks"].as_object().unwrap();
    for event in ["SessionStart", "UserPromptSubmit", "PostToolUse"] {
        assert!(events.contains_key(event), "hooks.json is missing {event}");
    }
    for (event, groups) in events {
        for group in groups.as_array().unwrap() {
            for hook in group["hooks"].as_array().unwrap() {
                let command = hook["command"].as_str().unwrap();
                let mut words = command.split_whitespace();
                assert_eq!(words.next(), Some("marshal-shim"), "{command}");
                let args: Vec<&str> = words.collect();
                assert_eq!(args.first(), Some(&"claude-hook"), "{command}");

                let (base, _seen) = fake_hook_listener("<marshal_session/>");
                let stdout = run_hook(&args, &base);
                additional_context(&stdout, event);
            }
        }
    }
}
