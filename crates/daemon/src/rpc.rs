use crate::conn::AppState;
use proto::messages::{ErrorCode, ServerMsg};
use std::sync::Arc;

pub async fn dispatch(
    _app: &Arc<AppState>,
    _session_id: &str,
    id: u64,
    method: &str,
    _params: serde_json::Value,
) -> ServerMsg {
    ServerMsg::RpcErr {
        id,
        code: ErrorCode::BadRequest,
        message: format!("method '{method}' not yet implemented"),
    }
}
