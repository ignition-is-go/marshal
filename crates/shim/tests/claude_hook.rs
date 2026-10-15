//! `marshal-shim claude-hook` forwards Claude Code's hook JSON to the daemon's
//! `/hook/*` endpoint and hands the daemon's answer back as the context Claude
//! adds to the turn. A fake daemon records each request and answers with a
//! fixed body, so these run the real binary without a real daemon.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use serde_json::Value;

const SHIM: &str = env!("CARGO_BIN_EXE_marshal-shim");
const HOOK_INPUT: &str = r#"{"session_id":"claude-hook-test","cwd":"/work/demo","hook_event_name":"UserPromptSubmit","prompt":"hi"}"#;

struct Request {
    line: String,
    body: String,
}

/// A one-request fake daemon hook listener. Returns its base URL and a
/// receiver for the request it saw.
fn fake_daemon(answer: &'static str) -> (String, mpsc::Receiver<Request>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake daemon");
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (seen_tx, seen_rx) = mpsc::channel();
    thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept hook request");
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).expect("request line");
        let mut length = 0;
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).expect("header");
            if header.trim().is_empty() {
                break;
            }
            if let Some((name, value)) = header.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse().expect("content length");
            }
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).expect("request body");
        let mut stream = reader.into_inner();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}",
            answer.len()
        )
        .expect("write answer");
        let _ = seen_tx.send(Request {
            line: line.trim_end().to_string(),
            body: String::from_utf8(body).expect("utf-8 body"),
        });
    });
    (base, seen_rx)
}

fn claude_hook(event: &str, base: &str) -> String {
    let mut child = Command::new(SHIM)
        .args(["claude-hook", event])
        .env("MARSHAL_BASE_URL", base)
        .env("MARSHAL_HOST", "demo-host")
        .env("MARSHAL_OPERATOR", "demo-operator")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn marshal-shim claude-hook");
    child
        .stdin
        .take()
        .expect("hook stdin")
        .write_all(HOOK_INPUT.as_bytes())
        .expect("write hook input");
    let out = child.wait_with_output().expect("wait for claude-hook");
    assert!(out.status.success(), "claude-hook exited {:?}", out.status);
    String::from_utf8(out.stdout).expect("utf-8 stdout")
}

#[test]
fn a_prompt_hook_surfaces_the_inbox_as_prompt_context() {
    let (base, seen) = fake_daemon("<marshal_inbox>\n- from peer: hello\n</marshal_inbox>\n");

    let out = claude_hook("prompt-submit", &base);

    let request = seen
        .recv_timeout(Duration::from_secs(5))
        .expect("no request");
    assert!(
        request
            .line
            .starts_with("POST /hook/prompt-submit?host=demo-host&operator=demo-operator"),
        "{}",
        request.line
    );
    assert!(
        request.line.contains("&harness=claude"),
        "Claude must not get Codex's asSession guidance: {}",
        request.line
    );
    let body: Value = serde_json::from_str(&request.body).expect("JSON request body");
    assert_eq!(body["session_id"], "claude-hook-test");
    assert_eq!(body["cwd"], "/work/demo");

    let output: Value = serde_json::from_str(out.trim()).expect("JSON hook output");
    assert_eq!(
        output["hookSpecificOutput"]["hookEventName"],
        "UserPromptSubmit"
    );
    assert_eq!(
        output["hookSpecificOutput"]["additionalContext"],
        "<marshal_inbox>\n- from peer: hello\n</marshal_inbox>"
    );
}

#[test]
fn a_session_start_hook_surfaces_identity_as_session_context() {
    let (base, seen) = fake_daemon("<marshal_session nickname=\"x\" id=\"y\"/>\n");

    let out = claude_hook("session-start", &base);

    let request = seen
        .recv_timeout(Duration::from_secs(5))
        .expect("no request");
    assert!(
        request.line.starts_with("POST /hook/session-start?"),
        "{}",
        request.line
    );
    let output: Value = serde_json::from_str(out.trim()).expect("JSON hook output");
    assert_eq!(
        output["hookSpecificOutput"]["hookEventName"],
        "SessionStart"
    );
}

#[test]
fn an_empty_inbox_adds_nothing() {
    let (base, seen) = fake_daemon("");
    assert_eq!(claude_hook("prompt-submit", &base), "");
    seen.recv_timeout(Duration::from_secs(5))
        .expect("the hook must still have asked the daemon");
}

#[test]
fn an_unreachable_daemon_adds_nothing_and_does_not_fail_the_turn() {
    // Port 1: nothing listens, so the connect fails at once.
    assert_eq!(claude_hook("prompt-submit", "http://127.0.0.1:1"), "");
}

/// Every hook the Claude plugin ships runs this binary and answers for the
/// event it is registered under.
#[test]
fn the_plugin_hooks_run_the_shim_for_their_own_event() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../plugins/marshal-shim/hooks/hooks.json"
    );
    let config: Value =
        serde_json::from_str(&std::fs::read_to_string(path).expect("read plugin hooks.json"))
            .expect("plugin hooks.json is JSON");
    let events = config["hooks"].as_object().expect("a hooks map");
    for expected in ["SessionStart", "UserPromptSubmit"] {
        assert!(events.contains_key(expected), "no {expected} hook");
    }
    for (event, groups) in events {
        for hook in groups
            .as_array()
            .expect("matcher groups")
            .iter()
            .flat_map(|group| group["hooks"].as_array().expect("hooks").iter())
        {
            let command = hook["command"].as_str().expect("a command hook");
            let argv: Vec<&str> = command.split_whitespace().collect();
            assert_eq!(argv.first(), Some(&"marshal-shim"), "{command}");
            assert_eq!(argv.get(1), Some(&"claude-hook"), "{command}");
            let (base, _seen) = fake_daemon("context\n");
            let out = claude_hook(argv[2], &base);
            let output: Value = serde_json::from_str(out.trim()).expect("JSON hook output");
            assert_eq!(
                output["hookSpecificOutput"]["hookEventName"],
                event.as_str(),
                "{command}"
            );
        }
    }
}

#[test]
fn a_daemon_that_never_answers_holds_the_prompt_only_briefly() {
    // Accepts the connection, then says nothing.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind silent daemon");
    let base = format!("http://{}", listener.local_addr().unwrap());
    let silent = thread::spawn(move || {
        let held = listener.accept();
        thread::sleep(Duration::from_secs(6));
        drop(held);
    });

    let started = std::time::Instant::now();
    assert_eq!(claude_hook("prompt-submit", &base), "");
    let waited = started.elapsed();
    assert!(
        waited < Duration::from_secs(4),
        "held the prompt for {waited:?}"
    );
    drop(silent);
}
