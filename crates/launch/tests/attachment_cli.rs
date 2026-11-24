use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::Command;

use agent_attachment_protocol::{AttachmentResolution, Observation};
use marshal_launch::api::GET_LAUNCH_PATH;
use serde_json::{Value, json};

const ID: &str = "launch-0123456789abcdef0123456789abcdef";

fn view(state: &str, plan: Value) -> Value {
    json!({
        "launch_id": ID, "backend": "fixture", "name": "fixture",
        "process": state, "session_id": null, "task": {"state": "pending"}, "error": null,
        "launched": {"handle": "exact-worker", "host": "fixture", "cwd": "/work",
            "where": "fixture", "attach": "this shell text must never execute",
            "attachment_plan": plan}
    })
}

fn server(views: Vec<Value>) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let thread = std::thread::spawn(move || {
        for response in views {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut stream = BufReader::new(stream);
            let mut line = String::new();
            stream.read_line(&mut line).unwrap();
            assert_eq!(line.trim_end(), format!("POST {GET_LAUNCH_PATH} HTTP/1.1"));
            let mut length = 0;
            loop {
                line.clear();
                assert_ne!(stream.read_line(&mut line).unwrap(), 0);
                if line == "\r\n" {
                    break;
                }
                if let Some((key, value)) = line.split_once(':')
                    && key.eq_ignore_ascii_case("content-length")
                {
                    length = value.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; length];
            stream.read_exact(&mut body).unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(&body).unwrap(),
                json!({"launch_id": ID})
            );
            let body = response.to_string();
            write!(
                stream.get_mut(),
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        }
    });
    (base, thread)
}

fn cli(operation: &str, base: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_marshal-launch"))
        .args([operation, ID, "--daemon", base])
        .output()
        .unwrap()
}

fn plan() -> Value {
    json!({"kind":"local_process", "program":"owner-cli", "args":["attach", "exact-worker", "a b", "a'\"b"]})
}

#[test]
fn resolution_returns_owner_command_and_exact_origin_without_start_or_task_delivery() {
    let (base, thread) = server(vec![view("running", plan())]);
    let output = cli("attachment-plan", &base);
    thread.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let resolution: AttachmentResolution = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        serde_json::to_value(resolution).unwrap(),
        json!({
            "version":1,
            "target":{"kind":"managed", "authority":"marshal", "origin":base, "agent_id":ID},
            "observation":{"state":"attachable", "plan":{"kind":"local_process", "program":"marshal-launch",
                "args":["attach",ID,"--daemon",base]}}
        })
    );
}

#[test]
fn legacy_missing_plan_is_unsupported_and_never_uses_display_text() {
    let mut legacy = view("running", Value::Null);
    legacy["launched"]
        .as_object_mut()
        .unwrap()
        .remove("attachment_plan");
    let (base, thread) = server(vec![legacy.clone(), legacy]);
    let output = cli("attachment-plan", &base);
    let resolution: AttachmentResolution = serde_json::from_slice(&output.stdout).unwrap();
    assert!(matches!(
        resolution.observation,
        Observation::Unsupported { .. }
    ));
    let output = cli("attach", &base);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unsupported"));
    thread.join().unwrap();
}

#[test]
fn attach_rechecks_status_after_resolution_and_never_launches_a_replacement() {
    for state in ["stopped", "unknown", "exited", "absent", "not_started"] {
        let (base, thread) = server(vec![view("running", plan()), view(state, plan())]);
        assert!(cli("attachment-plan", &base).status.success());
        let output = cli("attach", &base);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("not confirmed running"));
        thread.join().unwrap();
    }
}

#[test]
fn refuses_a_service_response_for_a_different_launch() {
    let mut wrong = view("running", plan());
    wrong["launch_id"] = json!("other-launch");
    let (base, thread) = server(vec![wrong]);
    assert!(!cli("attach", &base).status.success());
    thread.join().unwrap();
}

#[cfg(unix)]
#[test]
fn attach_executes_native_and_pulse_clients_with_literal_args_and_sanitized_environment() {
    use std::os::unix::fs::PermissionsExt;
    for client in ["tmux client", "ssh client", "pulse-agents"] {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join(client);
        let result = directory.path().join("result");
        std::fs::write(
            &executable,
            r#"#!/bin/sh
[ -z "$MYKO_POSTGRES_URL$MARSHAL_AUTH_POSTGRES_URL$MARSHAL_AUTH_CLIENT_SECRET" ] || exit 43
[ "$PULSE_ATTACHMENT_TEST_AUTH" = synthetic-owner-auth ] || exit 44
printf '%s\n' "$2" "$3" > "$1"
"#,
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let plan =
            json!({"kind":"local_process", "program":executable, "args":[result,"a b","a'\"$b"]});
        let (base, thread) = server(vec![view("running", plan)]);
        let output = Command::new(env!("CARGO_BIN_EXE_marshal-launch"))
            .args(["attach", ID, "--daemon", &base])
            .env("PULSE_ATTACHMENT_TEST_AUTH", "synthetic-owner-auth")
            .envs(
                marshal_launch::external::DAEMON_CREDENTIAL_ENV
                    .iter()
                    .map(|key| (*key, "synthetic-sentinel")),
            )
            .output()
            .unwrap();
        thread.join().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(std::fs::read_to_string(result).unwrap(), "a b\na'\"$b\n");
    }
}
