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
    run_hook_with_agent(args, base, None)
}

fn run_hook_with_agent(args: &[&str], base: &str, agent: Option<&str>) -> String {
    let mut command = Command::new(SHIM);
    command.env_remove("MARSHAL_AGENT_ID");
    if let Some(agent) = agent {
        command.env("MARSHAL_AGENT_ID", agent);
    }
    let mut child = command
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
    let mut waiter_events = Vec::new();
    for (event, groups) in events {
        for group in groups.as_array().unwrap() {
            for hook in group["hooks"].as_array().unwrap() {
                let command = hook["command"].as_str().unwrap();
                let mut words = command.split_whitespace();
                assert_eq!(words.next(), Some("marshal-shim"), "{command}");
                let args: Vec<&str> = words.collect();
                assert_eq!(args.first(), Some(&"claude-hook"), "{command}");

                // The waiter answers by exiting 2, not with JSON; it has its
                // own tests below.
                if args.get(1) == Some(&"wait") {
                    assert_eq!(hook["asyncRewake"], true, "{event}: {command}");
                    waiter_events.push(event.as_str());
                    continue;
                }

                let (base, _seen) = fake_hook_listener("<marshal_session/>");
                let stdout = run_hook(&args, &base);
                additional_context(&stdout, event);
            }
        }
    }
    waiter_events.sort_unstable();
    assert_eq!(waiter_events, ["SessionStart", "Stop"]);
}

/// A config dir of its own, so a test's waiter lock stays out of the real one.
fn temp_config(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("marshal-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn spawn_waiter(base: &str, config: &std::path::Path) -> std::process::Child {
    let mut child = Command::new(SHIM)
        .args(["claude-hook", "wait"])
        .env("MARSHAL_AGENT_ID", " waiter/\"agent\nopaque ")
        .env("MARSHAL_BASE_URL", base)
        .env("XDG_CONFIG_HOME", config)
        .env("APPDATA", config)
        .env("MARSHAL_WAITER_LOCK_WAIT_SECS", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn marshal-shim claude-hook wait");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(HOOK_INPUT.as_bytes())
        .unwrap();
    child
}

#[test]
fn waiter_prints_mail_to_stderr_and_exits_2() {
    let inbox = "<marshal_inbox count=\"1\">\n- from peer: hi\n</marshal_inbox>\n";
    let (base, seen) = fake_hook_listener(inbox);
    let config = temp_config("waiter-mail");
    let out = spawn_waiter(&base, &config).wait_with_output().unwrap();
    let seen = seen.recv_timeout(Duration::from_secs(5)).unwrap();

    assert!(
        seen.request_line.starts_with("POST /hook/wait?"),
        "{}",
        seen.request_line
    );
    let body: serde_json::Value = serde_json::from_str(&seen.body).unwrap();
    assert_eq!(body["agent_id"], "waiter/\"agent\nopaque");
    assert!(!seen.request_line.contains("agent_id"));
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(
        String::from_utf8(out.stderr).unwrap().trim_end(),
        inbox.trim_end()
    );
}

/// Answers each of `count` connections with 403.
fn refusing_listener(count: usize) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for _ in 0..count {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 2048];
            let _ = std::io::Read::read(&mut stream, &mut buf);
            stream
                .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
        }
    });
    base
}

#[test]
fn a_refused_waiter_tells_the_session_once_per_cause() {
    let base = refusing_listener(2);
    let config = temp_config("waiter-refused");

    let first = spawn_waiter(&base, &config).wait_with_output().unwrap();
    assert_eq!(first.status.code(), Some(2));
    let stderr = String::from_utf8(first.stderr).unwrap();
    assert!(stderr.contains("403"), "{stderr}");

    // Stop runs the hook again: no second wake, or it would loop.
    let second = spawn_waiter(&base, &config).wait_with_output().unwrap();
    assert_eq!(second.status.code(), Some(0));
    assert!(second.stderr.is_empty());
}

#[test]
fn a_refused_watch_says_so_on_stdout_where_monitor_sees_it() {
    let base = refusing_listener(1);
    let config = temp_config("watch-refused");
    let mut child = Command::new(SHIM)
        .args(["watch", "11111111-1111-4111-8111-111111111111", &base])
        .env("XDG_CONFIG_HOME", &config)
        .env("APPDATA", &config)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn marshal-shim watch");
    let mut line = String::new();
    std::io::BufRead::read_line(
        &mut std::io::BufReader::new(child.stdout.take().unwrap()),
        &mut line,
    )
    .unwrap();
    let _ = child.kill();
    let _ = child.wait();
    assert!(line.contains("403"), "{line}");
}

#[test]
fn second_waiter_for_a_session_exits_quietly() {
    // Accepts the first waiter's connection and never answers, so it stays.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let config = temp_config("waiter-lock");
    let mut first = spawn_waiter(&base, &config);
    let _held = listener.accept().unwrap();

    let second = spawn_waiter(&base, &config).wait_with_output().unwrap();
    let _ = first.kill();
    let _ = first.wait();
    assert_eq!(second.status.code(), Some(0));
    assert!(second.stderr.is_empty());
}

fn boundary_listener(response: Vec<u8>, delay: Duration) -> (String, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0; 4096];
        let _ = stream.read(&mut request);
        for byte in response {
            if stream.write_all(&[byte]).is_err() {
                break;
            }
            if !delay.is_zero() {
                thread::sleep(delay);
            }
        }
        drop(stream);
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_millis(300);
        while Instant::now() < deadline {
            if let Ok((mut stream, _)) = listener.accept() {
                let n = stream.read(&mut request).unwrap_or(0);
                let _ = tx.send(String::from_utf8_lossy(&request[..n]).into_owned());
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
            }
            thread::sleep(Duration::from_millis(5));
        }
    });
    (base, rx)
}

#[test]
fn an_incomplete_reply_never_surfaces_or_acknowledges_mail() {
    let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\nX-Marshal-Ack-V1: [\"m1\",\"m2\"]\r\n\r\npartial";
    let (base, requests) = boundary_listener(raw.to_vec(), Duration::ZERO);
    assert_eq!(run_hook(&["claude-hook", "prompt-submit"], &base), "");
    assert!(requests.recv_timeout(Duration::from_millis(500)).is_err());
}

#[test]
fn a_trickled_reply_cannot_extend_the_whole_hook_deadline() {
    let raw =
        b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\nX-Marshal-Ack-V1: [\"m1\"]\r\n\r\npartial";
    let (base, requests) = boundary_listener(raw.to_vec(), Duration::from_millis(80));
    let started = Instant::now();
    assert_eq!(run_hook(&["claude-hook", "prompt-submit"], &base), "");
    assert!(started.elapsed() < Duration::from_secs(4));
    assert!(requests.recv_timeout(Duration::from_millis(500)).is_err());
}

#[test]
fn a_closed_monitor_pipe_leaves_delivered_mail_unacknowledged() {
    let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nX-Marshal-Ack-V1: [\"m1\"]\r\n\r\nhello";
    let (base, requests) = boundary_listener(raw.to_vec(), Duration::ZERO);
    let mut child = Command::new(SHIM)
        .args(["watch", "boundary-test", &base, "--once"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    assert!(child.wait().unwrap().success());
    assert!(requests.recv_timeout(Duration::from_millis(500)).is_err());
}

#[test]
fn an_oversized_response_is_rejected_before_reading_its_body() {
    let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 262145\r\nX-Marshal-Ack-V1: [\"m1\"]\r\n\r\n";
    let (base, requests) = boundary_listener(raw.to_vec(), Duration::ZERO);
    assert_eq!(run_hook(&["claude-hook", "prompt-submit"], &base), "");
    assert!(requests.recv_timeout(Duration::from_millis(500)).is_err());
}

#[test]
fn a_replacement_waiter_takes_over_after_the_old_process_releases_its_lock() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let config = temp_config("waiter-takeover");
    let mut old = spawn_waiter(&base, &config);
    let (held, _) = listener.accept().unwrap();
    let mut replacement = Command::new(SHIM)
        .args(["claude-hook", "wait"])
        .env("MARSHAL_BASE_URL", &base)
        .env("XDG_CONFIG_HOME", &config)
        .env("APPDATA", &config)
        .env("MARSHAL_WAITER_LOCK_WAIT_SECS", "3")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    replacement
        .stdin
        .take()
        .unwrap()
        .write_all(HOOK_INPUT.as_bytes())
        .unwrap();
    thread::sleep(Duration::from_millis(200));
    assert!(replacement.try_wait().unwrap().is_none());
    old.kill().unwrap();
    old.wait().unwrap();
    drop(held);
    listener.set_nonblocking(true).unwrap();
    let until = Instant::now() + Duration::from_secs(3);
    let (mut stream, _) = loop {
        if let Ok(pair) = listener.accept() {
            break pair;
        }
        assert!(
            Instant::now() < until,
            "replacement never acquired the released waiter lock"
        );
        thread::sleep(Duration::from_millis(10));
    };
    let mut request = [0; 4096];
    assert!(stream.read(&mut request).unwrap() > 0);
    let reply = "<marshal_inbox/>";
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{reply}",
        reply.len()
    )
    .unwrap();
    let out = replacement.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(String::from_utf8(out.stderr).unwrap().trim(), reply);
}

#[test]
fn both_harness_hooks_transport_normalized_metadata_only_in_json_body() {
    for harness in ["codex-hook", "claude-hook"] {
        for value in [None, Some(""), Some(" \t "), Some(" agent/\"x\nopaque ")] {
            let (base, seen) = fake_hook_listener("");
            run_hook_with_agent(&[harness, "session-start"], &base, value);
            let seen = seen.recv_timeout(Duration::from_secs(5)).unwrap();
            let body: serde_json::Value = serde_json::from_str(&seen.body).unwrap();
            let expected = value.map(str::trim).filter(|s| !s.is_empty());
            assert_eq!(body["agent_id"].as_str(), expected);
            assert_eq!(body["session_id"], "itest-claude");
            assert_eq!(body["cwd"], "/tmp/proj");
            assert!(!seen.request_line.contains("agent_id"));
            assert!(!seen.request_line.contains("opaque"));
        }
    }
}
