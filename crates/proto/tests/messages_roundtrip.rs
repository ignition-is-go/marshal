use proto::messages::{ClientMsg, ServerMsg};
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
