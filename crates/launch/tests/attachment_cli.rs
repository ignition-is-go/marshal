use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::Command;

use agent_attachment_protocol::{AttachmentResolution, Observation};
use marshal_launch::api::GET_LAUNCH_PATH;
use serde_json::{Value, json};

const ID: &str = "launch-0123456789abcdef0123456789abcdef";

fn view(state: &str, plan: Value) -> Value {
    let plan = if plan.is_null() || plan.get("execution").is_some() {
        plan
    } else {
        json!({"execution":"client_local", "plan":plan})
    };
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
if [ "$4" = native ]; then
[ -z "$MYKO_POSTGRES_URL$MARSHAL_AUTH_POSTGRES_URL$MARSHAL_AUTH_CLIENT_SECRET" ] || exit 43
else
[ "$MYKO_POSTGRES_URL" = synthetic-sentinel ] || exit 45
fi
[ "$PULSE_ATTACHMENT_TEST_AUTH" = synthetic-owner-auth ] || exit 44
printf '%s\n' "$2" "$3" > "$1"
"#,
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let plan = json!({"kind":"local_process", "program":executable, "args":[result,"a b","a'\"$b", if client == "pulse-agents" { "provider" } else { "native" }]});
        let plan = json!({"execution": if client == "pulse-agents" { "client_local" } else { "owner_local" }, "plan":plan});
        let (base, thread) = server(vec![view("running", plan)]);
        let output = Command::new(env!("CARGO_BIN_EXE_marshal-launch"))
            .args(["attach", ID, "--daemon", &base, "--owner-local"])
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

#[test]
fn native_context_requires_explicit_owner_route_even_for_loopback_origins() {
    let native = json!({"execution":"owner_local", "plan":plan()});
    let (base, thread) = server(vec![
        view("running", native.clone()),
        view("running", native),
    ]);
    let result = cli("attachment-plan", &base);
    let resolution: AttachmentResolution = serde_json::from_slice(&result.stdout).unwrap();
    assert!(
        matches!(resolution.observation, Observation::Unsupported { reason } if reason.contains("--on"))
    );
    let result = cli("attach", &base);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("--owner-local"));
    thread.join().unwrap();
}

#[test]
fn caller_ssh_route_is_retained_in_the_public_plan() {
    let native = json!({"execution":"owner_local", "plan":plan()});
    let (base, thread) = server(vec![view("running", native)]);
    let output = Command::new(env!("CARGO_BIN_EXE_marshal-launch"))
        .args([
            "attachment-plan",
            ID,
            "--daemon",
            &base,
            "--on",
            "user@owner",
            "--owner-daemon",
            "http://owner:6156",
            "--owner-program",
            "/opt/my tools/marshal-launch",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let result: AttachmentResolution = serde_json::from_slice(&output.stdout).unwrap();
    let Observation::Attachable { plan } = result.observation else {
        panic!("not attachable")
    };
    assert_eq!(
        plan.command().args,
        vec![
            "attach",
            ID,
            "--daemon",
            &base,
            "--on",
            "user@owner",
            "--owner-program",
            "/opt/my tools/marshal-launch",
            "--owner-daemon",
            "http://owner:6156"
        ]
    );
    thread.join().unwrap();
}

#[cfg(unix)]
#[test]
fn ssh_owner_route_rechecks_the_owner_origin_then_executes_owner_local() {
    use std::os::unix::fs::PermissionsExt;
    for forwarded in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let ssh = directory.path().join("ssh");
        std::fs::write(&ssh, "#!/bin/sh\n[ \"$1\" = -t ] && [ \"$2\" = -- ] && [ \"$3\" = owner-alias ] || exit 41\nexec sh -c \"$4\"\n").unwrap();
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
        let native = json!({"execution":"owner_local", "plan":{"kind":"local_process", "program":"/bin/echo", "args":["owner exact target"]}});
        let (client_origin, client) = server(if forwarded {
            vec![view("running", native.clone())]
        } else {
            vec![
                view("running", native.clone()),
                view("running", native.clone()),
            ]
        });
        let owner = if forwarded {
            Some(server(vec![view("running", native)]))
        } else {
            None
        };
        let path = format!(
            "{}:{}",
            directory.path().display(),
            std::env::var("PATH").unwrap()
        );
        let mut command = Command::new(env!("CARGO_BIN_EXE_marshal-launch"));
        command.args([
            "attach",
            ID,
            "--daemon",
            &client_origin,
            "--on",
            "owner-alias",
            "--owner-program",
            env!("CARGO_BIN_EXE_marshal-launch"),
        ]);
        if let Some((origin, _)) = &owner {
            command.args(["--owner-daemon", origin]);
        }
        let output = command.env("PATH", path).output().unwrap();
        client.join().unwrap();
        if let Some((_, thread)) = owner {
            thread.join().unwrap();
        }
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            "owner exact target\n"
        );
    }
}
