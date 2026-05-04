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
        other => err(id, ErrorCode::BadRequest, format!("unknown method '{other}'")),
    }
}

fn ok<T: serde::Serialize>(id: u64, value: T) -> ServerMsg {
    ServerMsg::RpcOk { id, result: serde_json::to_value(value).unwrap() }
}

fn err(id: u64, code: ErrorCode, message: String) -> ServerMsg {
    ServerMsg::RpcErr { id, code, message }
}
