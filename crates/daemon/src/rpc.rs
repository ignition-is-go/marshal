use crate::conn::AppState;
use proto::messages::{ErrorCode, ServerMsg};
use proto::rpc::{
    method, OkResult, RosterParams, RosterResult, SetStatusParams,
};
use std::sync::Arc;

pub async fn dispatch(
    app: &Arc<AppState>,
    session_id: &str,
    id: u64,
    method_name: &str,
    params: serde_json::Value,
) -> ServerMsg {
    match method_name {
        method::ROSTER => match serde_json::from_value::<RosterParams>(params) {
            Ok(_) => {
                let sessions = app.roster.snapshot(session_id);
                ok(id, RosterResult { sessions })
            }
            Err(e) => err(id, ErrorCode::BadRequest, e.to_string()),
        },
        method::SET_STATUS => match serde_json::from_value::<SetStatusParams>(params) {
            Ok(p) => {
                app.roster.set_status(session_id, p.text);
                ok(id, OkResult { ok: true })
            }
            Err(e) => err(id, ErrorCode::BadRequest, e.to_string()),
        },
        method::SEND_MESSAGE => match serde_json::from_value::<proto::rpc::SendMessageParams>(params) {
            Ok(p) => match app.roster.resolve(&p.to) {
                Ok((to_session, to_nick)) => {
                    let from_nick = app.roster
                        .snapshot(session_id)
                        .into_iter()
                        .find(|s| s.session_id == session_id)
                        .map(|s| s.nickname)
                        .unwrap_or_else(|| "?".into());
                    let now = crate::conn::now_ms();
                    let store = app.store.lock().await;
                    match store.insert_message(session_id, &from_nick, &to_session, &to_nick, &p.body, now) {
                        Ok(message_id) => ok(id, proto::rpc::SendMessageResult { message_id }),
                        Err(e) => err(id, ErrorCode::Internal, e.to_string()),
                    }
                }
                Err(crate::state::ResolveError::Unknown(_)) =>
                    err(id, ErrorCode::UnknownRecipient,
                        format!("no live session named '{}'", p.to)),
                Err(crate::state::ResolveError::Ambiguous(ids)) =>
                    err(id, ErrorCode::AmbiguousRecipient,
                        format!("multiple live sessions named '{}': {:?}", p.to, ids)),
            },
            Err(e) => err(id, ErrorCode::BadRequest, e.to_string()),
        },
        method::INBOX => match serde_json::from_value::<proto::rpc::InboxParams>(params) {
            Ok(p) => {
                let store = app.store.lock().await;
                match store.unread_for(session_id) {
                    Ok(rows) => {
                        let messages: Vec<proto::messages::Message> = rows.iter().map(|r| {
                            proto::messages::Message {
                                id: r.id, from_session: r.from_session.clone(),
                                from_nick: r.from_nick.clone(), body: r.body.clone(),
                                sent_at: r.sent_at,
                            }
                        }).collect();
                        if p.mark_read {
                            let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
                            let _ = store.mark_read(&ids, crate::conn::now_ms());
                        }
                        ok(id, proto::rpc::InboxResult { messages })
                    }
                    Err(e) => err(id, ErrorCode::Internal, e.to_string()),
                }
            }
            Err(e) => err(id, ErrorCode::BadRequest, e.to_string()),
        },
        method::RECENT_MESSAGES => match serde_json::from_value::<proto::rpc::RecentMessagesParams>(params) {
            Ok(p) => {
                let store = app.store.lock().await;
                match store.recent_for(session_id, p.limit) {
                    Ok(rows) => {
                        let messages = rows.into_iter().map(|r| {
                            let direction = if r.from_session == session_id {
                                proto::rpc::Direction::Sent
                            } else {
                                proto::rpc::Direction::Received
                            };
                            proto::rpc::RecentMessage {
                                message: proto::messages::Message {
                                    id: r.id, from_session: r.from_session,
                                    from_nick: r.from_nick, body: r.body, sent_at: r.sent_at,
                                },
                                direction,
                                to_nick: r.to_nick,
                            }
                        }).collect();
                        ok(id, proto::rpc::RecentMessagesResult { messages })
                    }
                    Err(e) => err(id, ErrorCode::Internal, e.to_string()),
                }
            }
            Err(e) => err(id, ErrorCode::BadRequest, e.to_string()),
        },
        other => err(id, ErrorCode::BadRequest, format!("unknown method '{other}'")),
    }
}

fn ok<T: serde::Serialize>(id: u64, value: T) -> ServerMsg {
    ServerMsg::RpcOk { id, result: serde_json::to_value(value).unwrap() }
}

fn err(id: u64, code: ErrorCode, message: String) -> ServerMsg {
    ServerMsg::RpcErr { id, code, message }
}
