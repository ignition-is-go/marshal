use proto::rpc::*;

#[test]
fn set_status_params_serializes() {
    let p = SetStatusParams { text: "refactoring auth".to_string() };
    assert_eq!(serde_json::to_value(&p).unwrap(),
        serde_json::json!({"text": "refactoring auth"}));
}

#[test]
fn send_message_params_serializes() {
    let p = SendMessageParams { to: "eww".into(), body: "hi".into() };
    assert_eq!(serde_json::to_value(&p).unwrap(),
        serde_json::json!({"to": "eww", "body": "hi"}));
}

#[test]
fn inbox_params_default_marks_read() {
    let p = InboxParams::default();
    assert!(p.mark_read);
}

#[test]
fn recent_messages_params_default_limit() {
    let p = RecentMessagesParams::default();
    assert_eq!(p.limit, 50);
}
