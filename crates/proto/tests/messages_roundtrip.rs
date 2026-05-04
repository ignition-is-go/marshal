use proto::messages::{ClientMsg, ErrorCode, ServerMsg};
use std::path::PathBuf;

#[test]
fn hello_roundtrips() {
    let original = ClientMsg::Hello {
        pid: 1234,
        cwd: PathBuf::from("/home/trevor/Code/eww"),
        git_branch: Some("main".to_string()),
    };
    let json = serde_json::to_string(&original).unwrap();
    let parsed: ClientMsg = serde_json::from_str(&json).unwrap();
    assert_eq!(original, parsed);
}

#[test]
fn welcome_roundtrips() {
    let original = ServerMsg::Welcome {
        session_id: "s-7a3f".to_string(),
        nickname: "eww".to_string(),
    };
    let json = serde_json::to_string(&original).unwrap();
    let parsed: ServerMsg = serde_json::from_str(&json).unwrap();
    assert_eq!(original, parsed);
}

#[test]
fn hello_uses_tagged_form() {
    let msg = ClientMsg::Hello {
        pid: 1,
        cwd: PathBuf::from("/x"),
        git_branch: None,
    };
    let v: serde_json::Value = serde_json::to_value(&msg).unwrap();
    assert_eq!(v["type"], "hello");
}

#[test]
fn rpc_request_roundtrips() {
    let original = ClientMsg::Rpc {
        id: 42,
        method: "roster".to_string(),
        params: serde_json::json!({}),
    };
    let json = serde_json::to_string(&original).unwrap();
    let parsed: ClientMsg = serde_json::from_str(&json).unwrap();
    assert_eq!(original, parsed);
}

#[test]
fn rpc_ok_roundtrips() {
    let original = ServerMsg::RpcOk {
        id: 42,
        result: serde_json::json!({"ok": true}),
    };
    let parsed: ServerMsg = serde_json::from_str(&serde_json::to_string(&original).unwrap()).unwrap();
    assert_eq!(original, parsed);
}

#[test]
fn rpc_err_roundtrips() {
    let original = ServerMsg::RpcErr {
        id: 42,
        code: ErrorCode::UnknownRecipient,
        message: "no session named 'eww'".to_string(),
    };
    let parsed: ServerMsg = serde_json::from_str(&serde_json::to_string(&original).unwrap()).unwrap();
    assert_eq!(original, parsed);
}

#[test]
fn error_code_uses_snake_case() {
    let v = serde_json::to_value(ErrorCode::UnknownRecipient).unwrap();
    assert_eq!(v, serde_json::json!("unknown_recipient"));
}
