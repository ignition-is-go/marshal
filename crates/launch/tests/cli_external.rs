use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::Command;

use marshal_launch::api::{LAUNCH_PATH, StartLaunch};
use serde_json::json;

#[test]
fn cli_submits_to_the_general_launch_api() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let http = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut stream = BufReader::new(stream);
        let mut line = String::new();
        stream.read_line(&mut line).unwrap();
        assert_eq!(line.trim_end(), format!("POST {LAUNCH_PATH} HTTP/1.1"));
        let mut length = 0;
        loop {
            line.clear();
            assert_ne!(stream.read_line(&mut line).unwrap(), 0);
            if line == "\r\n" {
                break;
            }
            if let Some((key, val)) = line.split_once(':')
                && key.eq_ignore_ascii_case("Content-Length")
            {
                length = val.trim().parse().unwrap();
            }
        }
        let mut body = vec![0; length];
        stream.read_exact(&mut body).unwrap();
        let request: StartLaunch = serde_json::from_slice(&body).unwrap();
        assert_eq!(request.backend, "ssh");
        assert_eq!(request.harness, "claude");
        assert_eq!(request.cwd, "/work/app");
        assert_eq!(request.options, ["--host", "build-03"]);
        assert_eq!(request.first_task.as_deref(), Some("say ready"));
        let body = json!({
            "launch_id": request.launch_id, "backend": "ssh", "name": request.name,
            "process": "running", "launched": null, "session_id": "worker",
            "task": {"state": "stored", "message_id": "message-1"}, "error": null
        })
        .to_string();
        write!(
            stream.get_mut(),
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len(),
        )
        .unwrap();
    });
    let output = Command::new(env!("CARGO_BIN_EXE_marshal-launch"))
        .args([
            "ssh",
            "claude",
            "--daemon",
            &base,
            "--cwd",
            "/work/app",
            "--task",
            "say ready",
            "--",
            "--host",
            "build-03",
        ])
        .output()
        .unwrap();
    http.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response: marshal_launch::api::LaunchView = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response.session_id.as_deref(), Some("worker"));
}
