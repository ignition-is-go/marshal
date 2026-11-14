//! Real MCP requests must use the host thread identity, even with inherited arguments.
use marshal_entities::Session;
use myko::wire::{MEvent, MEventType};
use myko_server::{BlackholePersister, MykoServer};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Shim(Child);
impl Drop for Shim {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn request(shim: &mut Shim, reader: &mut impl BufRead, method: &str, params: Value) -> Value {
    writeln!(
        shim.0.stdin.as_mut().unwrap(),
        "{}",
        json!({
            "jsonrpc": "2.0", "id": 1, "method": method, "params": params,
        })
    )
    .unwrap();
    loop {
        let mut line = String::new();
        assert_ne!(
            reader.read_line(&mut line).unwrap(),
            0,
            "shim closed stdout"
        );
        let response: Value = serde_json::from_str(&line).unwrap();
        if response.get("id").is_some() {
            assert!(response.get("error").is_none(), "{response}");
            return response["result"].clone();
        }
    }
}
fn send(shim: &mut Shim, reader: &mut impl BufRead, thread: &str, to: &str, body: &str) {
    let result = request(
        shim,
        reader,
        "tools/call",
        json!({
            "name": "send_message", "arguments": {"asSession": "parent", "to": to, "body": body},
            "_meta": {"threadId": thread},
        }),
    );
    assert_eq!(result["isError"], false, "{result}");
}
fn resource(shim: &mut Shim, reader: &mut impl BufRead, uri: &str, thread: &str) -> Value {
    let result = request(
        shim,
        reader,
        "resources/read",
        json!({
            "uri": uri, "_meta": {"threadId": thread},
        }),
    );
    serde_json::from_str(result["contents"][0]["text"].as_str().unwrap()).unwrap()
}
fn inbox(shim: &mut Shim, reader: &mut impl BufRead, thread: &str) -> Value {
    resource(
        shim,
        reader,
        "marshal://messages?asSession=parent&inbox=true",
        thread,
    )
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fork_sender_and_return_address_are_bound_to_request_metadata() {
    marshal_entities::link();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let server = Arc::new(
        MykoServer::builder()
            .with_bind_addr(address)
            .with_default_persister(Arc::new(BlackholePersister))
            .build(),
    );
    for id in ["parent", "fork", "peer"] {
        let session: Session = serde_json::from_value(json!({
            "id": id, "pid": 0, "cwd": "/repo", "connectedAt": 1,
        }))
        .unwrap();
        server
            .ctx()
            .apply_event_batch(vec![MEvent::from_item(&session, MEventType::SET, "test")])
            .unwrap();
    }
    let run = tokio::spawn(async move { server.run().await });
    let config = tempfile::tempdir().unwrap();
    // Installed address files take precedence over env; isolate from the live daemon.
    let mut shim = Shim(
        Command::new(env!("CARGO_BIN_EXE_marshal-shim"))
            .env("XDG_CONFIG_HOME", config.path())
            .env("HOME", config.path())
            .env("APPDATA", config.path())
            .env("PROGRAMDATA", config.path())
            .env("MARSHAL_HARNESS", "codex")
            .env("MARSHAL_DAEMON_ADDRESS", format!("ws://{address}"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let mut reader = BufReader::new(shim.0.stdout.take().unwrap());
    request(
        &mut shim,
        &mut reader,
        "initialize",
        json!({
            "protocolVersion": "2024-11-05", "capabilities": {},
            "clientInfo": {"name": "fork-regression", "version": "1"},
        }),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let roster = resource(&mut shim, &mut reader, "marshal://roster", "fork");
        if roster["sessions"].as_array().unwrap().len() == 3 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "shim did not connect to isolated server"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    send(&mut shim, &mut reader, "fork", "peer", "from fork");
    send(&mut shim, &mut reader, "parent", "peer", "from parent");
    let messages = inbox(&mut shim, &mut reader, "peer");
    let messages = messages["messages"].as_array().unwrap();
    for (body, sender) in [("from fork", "fork"), ("from parent", "parent")] {
        let message = messages.iter().find(|m| m["body"] == body).unwrap();
        assert_eq!(message["fromSessionId"], sender);
    }
    send(&mut shim, &mut reader, "peer", "fork", "reply to fork");
    let fork = inbox(&mut shim, &mut reader, "fork");
    assert!(
        fork["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["body"] == "reply to fork")
    );
    let parent = inbox(&mut shim, &mut reader, "parent");
    assert!(parent["messages"].as_array().unwrap().is_empty());
    run.abort();
}
