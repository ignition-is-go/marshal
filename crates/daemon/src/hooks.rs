//! Agent hook logic — all of it server-side.
//!
//! These run behind marshal's own plain-HTTP listener (`http_listener`),
//! not myko's MCP endpoint: the hook command on every platform is a dumb
//! curl one-liner that POSTs Claude Code's raw hook JSON and prints the
//! `text/plain` response back into the agent's context.
//!
//! ```text
//! curl -sS --max-time 5 -X POST \
//!   "$URL/hook/session-start?host=$(hostname -s)&operator=$USER" \
//!   --data-binary @- || true
//! ```
//!
//! No client-side scripts, no jq/bash, no per-platform port — the
//! register / fetch / ack / format work happens here, once, in Rust.
//!
//! `host` / `operator` ride in the query string because the daemon is
//! remote and can't know the *client's* hostname or user; the curl
//! command expands them locally (the only platform-specific bit, `$VAR`
//! vs `%VAR%`). Everything else (`session_id`, `cwd`) is in the hook body.
//!
//! Caller identity for the read/ack commands is carried by the commands'
//! `asSession` field (self-identify), since this internal context has no
//! WS `client_id`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use myko::prelude::EventPublishing as _;
use myko::{
    command::{CommandContext, CommandHandler},
    request::RequestContext,
    server::MykoServerContext,
};
use serde_json::Value;

use marshal_entities::{
    AckMessages, CONTEXT_BODY_MAX_CHARS, GetAllSessions, GetAllWakeWatchers, HostInfo, MessageId,
    MessageView, ReadMessages, Session, SessionId, WakeWatcher, WakeWatcherId, context_preview,
    nickname_for,
};

const AUTO_INBOX_BODY_BUDGET_CHARS: usize = 8_000;
const AUTO_INBOX_MIN_BODY_CHARS: usize = 256;

/// A hook's HTTP response body, plus any inbox ack that must be deferred
/// until the response is confirmed written back to the caller.
///
/// Acking is what marks a surfaced message "delivered". Doing it inside the
/// hook — before the `<marshal_inbox>` bytes reach the agent — is at-most-once:
/// a response that times out (`curl --max-time 5`) or drops mid-write would
/// leave the messages marked read but never seen. So `surface_unread` returns
/// the surfaced ids here and the listener acks them ONLY after a successful
/// `write_all`+flush (see `ack_surfaced`), making inbox delivery at-least-once:
/// a failed write leaves them unread to re-surface next turn.
pub struct HookOutcome {
    pub body: String,
    pub deferred_ack: Option<(SessionId, Vec<MessageId>)>,
}

impl HookOutcome {
    fn text(body: String) -> Self {
        Self {
            body,
            deferred_ack: None,
        }
    }
}

/// How long surfaced mail stays out of other readers' way while its reader
/// delivers it. If no ack arrives by then, the path was lost and it's offered
/// again, so a slow reader can see a duplicate but mail isn't lost.
const DEFAULT_LEASE: Duration = Duration::from_secs(60);

/// How long a lapsed lease is remembered, so that offering its mail again can be
/// told apart from a first offer and logged.
const FORGET_LAPSED: Duration = Duration::from_secs(60 * 60);

/// Mail handed to a reader and not yet acked, as session and message to expiry.
#[derive(Default)]
struct Leases(HashMap<(SessionId, MessageId), Instant>);

/// What [`Leases::take`] handed out.
struct Taken {
    /// The messages leased to this reader.
    ids: HashSet<MessageId>,
    /// How many of them were offered before, and that reader never acked.
    reoffered: usize,
}

impl Leases {
    /// Lease whichever of `ids` nobody holds, returning those.
    fn take(
        &mut self,
        session: &SessionId,
        ids: &[MessageId],
        now: Instant,
        lease: Duration,
    ) -> Taken {
        self.0.retain(|_, expires| *expires + FORGET_LAPSED > now);
        let mut reoffered = 0;
        let ids = ids
            .iter()
            .filter(|id| {
                let key = (session.clone(), (*id).clone());
                match self.0.get(&key) {
                    Some(expires) if *expires > now => return false,
                    Some(_) => reoffered += 1,
                    None => {}
                }
                self.0.insert(key, now + lease);
                true
            })
            .cloned()
            .collect();
        Taken { ids, reoffered }
    }

    fn release(&mut self, session: &SessionId, ids: &[MessageId]) {
        for id in ids {
            self.0.remove(&(session.clone(), id.clone()));
        }
    }
}

static LEASES: LazyLock<Mutex<Leases>> = LazyLock::new(Mutex::default);

fn lease_duration() -> Duration {
    std::env::var("MARSHAL_ACK_LEASE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map_or(DEFAULT_LEASE, Duration::from_secs)
}

/// Dispatch a POST to a `/hook/*` path. Returns `Some(outcome)` for a known
/// hook route — the listener writes `outcome.body` as the `text/plain` body,
/// then runs `outcome.deferred_ack` — or `None` for an unknown path (→ 404).
pub fn dispatch(
    path: &str,
    query: &str,
    body: &[u8],
    ctx: &Arc<MykoServerContext>,
) -> Option<HookOutcome> {
    match path {
        "/hook/session-register" => Some(handle_session_register(query, body, ctx)),
        "/hook/session-start" => Some(handle_session_start(query, body, ctx)),
        "/hook/prompt-submit" => Some(handle_prompt_submit(query, body, ctx)),
        "/hook/session-end" => Some(handle_session_end(body, ctx)),
        "/hook/ack" => Some(handle_ack(body, ctx)),
        _ => None,
    }
}

/// Ack the messages a hook surfaced, AFTER its response was written. Called by
/// the listener on write success so a lost/timed-out response can't lose
/// messages (they stay unread and re-surface). Fail-loud: a failed ack is
/// logged, not silently swallowed.
pub fn ack_surfaced(ctx: &Arc<MykoServerContext>, session: &SessionId, ids: Vec<MessageId>) {
    if ids.is_empty() {
        return;
    }
    let cmd_ctx = internal_cmd_ctx(ctx);
    let acked = (AckMessages {
        message_ids: ids.clone(),
        as_session: Some(session.clone()),
    })
    .execute(cmd_ctx);
    // Only now, so the row is read before anyone else can be offered it.
    if let Ok(mut leases) = LEASES.lock() {
        leases.release(session, &ids);
    }
    if let Err(e) = acked {
        log::warn!(
            "[hook] deferred inbox ack failed for {}: {e:?}",
            session.0.as_ref()
        );
    }
}

/// A client acknowledging mail it has actually handed on (`/hook/wait?ack=client`).
fn handle_ack(body: &[u8], ctx: &Arc<MykoServerContext>) -> HookOutcome {
    let parsed = parse_body(body);
    let session = parsed
        .as_ref()
        .and_then(|b| b.get("session_id")?.as_str())
        .map(|sid| SessionId(Arc::from(sid)));
    let ids: Vec<MessageId> = parsed
        .as_ref()
        .and_then(|b| b.get("message_ids")?.as_array())
        .into_iter()
        .flatten()
        .filter_map(|id| id.as_str())
        .map(|id| MessageId(Arc::from(id)))
        .collect();
    if let Some(session) = session {
        if !ids.is_empty() {
            log::info!(
                "[hook] {}: client acked {} message(s)",
                session.0.as_ref(),
                ids.len()
            );
        }
        ack_surfaced(ctx, &session, ids);
    }
    HookOutcome::text(String::new())
}

/// Register or refresh a hook-owned session without surfacing its inbox.
///
/// Codex's app-server bridge calls this as soon as it observes
/// `thread/started`, before the TUI can submit its first prompt. Keeping this
/// separate from `/hook/session-start` is important: the eager registration
/// request has no model turn to receive context, so surfacing (and later
/// acknowledging) unread messages here would lose them.
fn handle_session_register(query: &str, body: &[u8], ctx: &Arc<MykoServerContext>) -> HookOutcome {
    let Some((sid, cmd_ctx)) = register_hook_session(query, body, ctx, Activity::Active) else {
        return HookOutcome::text(String::new());
    };
    let nickname = crate::nickname_assign::ensure_assigned(&cmd_ctx, &sid)
        .unwrap_or_else(|_| marshal_entities::nickname(&sid));
    HookOutcome::text(
        serde_json::json!({
            "session_id": sid,
            "nickname": nickname,
        })
        .to_string(),
    )
}

fn handle_session_start(query: &str, body: &[u8], ctx: &Arc<MykoServerContext>) -> HookOutcome {
    let Some((sid, cmd_ctx)) = register_hook_session(query, body, ctx, Activity::Active) else {
        return HookOutcome::text(String::new());
    };

    // Inject the agent's own marshal identity for context — recognising
    // itself in the roster, addressing self-sends. Persists in context across
    // the session; re-injected on resume. The `asSession` guidance is
    // harness-specific: Claude reaches marshal through the shim, which resolves
    // the sender from its WS connection, so the agent does NOT pass it. Codex's
    // MCP server has no connection identity (Codex never tells it the session),
    // so the Codex agent must name itself explicitly on every write.
    // The agent's own handle — so it recognises itself in the roster and can say
    // who it is. Authoritative (assigned handle, else the computed candidate the
    // assigner would use), matching what peers see in marshal://roster.
    let q = parse_query(query);
    let nick = nickname_for(&cmd_ctx, &sid).unwrap_or_else(|_| marshal_entities::nickname(&sid));
    let mut out = if q.get("harness").map(String::as_str) == Some("codex") {
        format!(
            "<marshal_session nickname=\"{nick}\" id=\"{sid}\">Use id as asSession on writes and \
             ?asSession= on caller-relative reads.</marshal_session>\n"
        )
    } else {
        // The `asyncRewake` hook `marshal-shim claude-hook wait` handles live
        // delivery, so the agent has nothing to keep armed.
        format!(
            "<marshal_session nickname=\"{nick}\" id=\"{sid}\">Marshal tools attach this \
             identity.</marshal_session>\n"
        )
    };
    let (inbox, ids) = surface_unread(&cmd_ctx, &sid);
    out.push_str(&inbox);
    HookOutcome {
        body: out,
        deferred_ack: (!ids.is_empty()).then(|| (SessionId(Arc::from(sid)), ids)),
    }
}

/// Whether a hook request is the agent doing something, which moves
/// `last_activity_at`, or only waiting for mail, which doesn't.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Activity {
    Active,
    Waiting,
}

/// Parse hook metadata and SET the corresponding pull-owned Session.
///
/// Returns the canonical session id plus the command context used for the SET
/// so `/hook/session-start` can immediately render identity and inbox state
/// against the same registry view.
fn register_hook_session(
    query: &str,
    body: &[u8],
    ctx: &Arc<MykoServerContext>,
    activity: Activity,
) -> Option<(String, CommandContext)> {
    let body = parse_body(body)?;
    let sid = body.get("session_id").and_then(|v| v.as_str())?;
    let sid = sid.to_string();
    let q = parse_query(query);
    let cwd = body
        .get("cwd")
        .and_then(|v| v.as_str())
        .or_else(|| {
            body.pointer("/workspace/current_dir")
                .and_then(|v| v.as_str())
        })
        .unwrap_or("")
        .to_string();
    // Recognise both `/` and `\` as path separators when extracting the
    // trailing component.
    let dir = cwd
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("session");
    let operator = q.get("operator").filter(|s| !s.is_empty()).cloned();
    let host = q.get("host").filter(|s| !s.is_empty()).map(|h| HostInfo {
        // `hostname` may return an FQDN (common on Windows); the host:*
        // auto-room keys on the short name, so drop the domain.
        name: h.split('.').next().unwrap_or(h).to_string(),
        os: q.get("os").cloned().unwrap_or_default(),
        arch: q.get("arch").cloned().unwrap_or_default(),
    });
    let project = if dir == "session" {
        None
    } else {
        Some(dir.to_string())
    };

    let cmd_ctx = internal_cmd_ctx(ctx);
    let existing: Vec<Arc<Session>> = cmd_ctx.exec_query(GetAllSessions {}).unwrap_or_default();
    let sid_typed = SessionId(Arc::from(sid.as_str()));
    let prior = existing.iter().find(|s| s.id == sid_typed);
    let now = chrono::Utc::now().timestamp_millis();
    let active_at = (activity == Activity::Active).then_some(now);
    // Preserve shim-owned fields (client_id, pid, git_branch, last_tool*)
    // when a prior row already exists. The hook can fire after the shim
    // has registered, and clobbering client_id back to None breaks live
    // notification routing — the failure mode we're explicitly avoiding
    // by sharing one session_id between hook and shim. The hook only
    // writes fields it uniquely sources (operator, host, project from
    // query string; cwd from payload; last_activity_at from "now", unless
    // the request is a wait).
    let session = match prior {
        Some(p) => {
            let mut updated = (**p).clone();
            updated.cwd = cwd;
            if active_at.is_some() {
                updated.last_activity_at = active_at;
            }
            if updated.operator.is_none() {
                updated.operator = operator;
            }
            if updated.host.is_none() {
                updated.host = host;
            }
            if updated.project.is_none() {
                updated.project = project;
            }
            updated
        }
        None => Session {
            id: sid_typed,
            client_id: None,
            pid: 0,
            cwd,
            git_branch: None,
            current_task: None,
            // Codex has no per-PID name manifest; identity is hook-owned.
            session_name: None,
            activity: None,
            kind: None,
            connected_at: now,
            last_activity_at: active_at,
            last_tool: None,
            last_tool_at: None,
            operator,
            host,
            project,
            channels_enabled: None,
        },
    };
    if let Err(e) = cmd_ctx.emit_set(&session) {
        log::warn!("[hook] session SET failed for {sid}: {e:?}");
    }
    Some((sid, cmd_ctx))
}

fn handle_prompt_submit(query: &str, body: &[u8], ctx: &Arc<MykoServerContext>) -> HookOutcome {
    // A live Codex TUI may outlast a daemon deployment or an older daemon's
    // time-based hook-session cleanup. Re-run the same idempotent registration
    // used by SessionStart so this prompt repairs a missing row in place before
    // inbox lookup. Codex supplies the authoritative session_id + cwd in every
    // hook body; host/operator remain on the hook query string.
    let Some((sid, cmd_ctx)) = register_hook_session(query, body, ctx, Activity::Active) else {
        return HookOutcome::text(String::new());
    };

    let (inbox, ids) = surface_unread(&cmd_ctx, &sid);
    HookOutcome {
        body: inbox,
        deferred_ack: (!ids.is_empty()).then(|| (SessionId(Arc::from(sid)), ids)),
    }
}

/// Start a `/hook/wait` long-poll: register (or refresh) the waiting session
/// once, returning the context the listener re-checks the inbox against.
///
/// A wait isn't activity. The waiter asks again each time one ends, so if it
/// moved `last_activity_at`, an idle session would look active: operator mail
/// would go to whichever session polled last, and the stale-session sweep
/// would never reap it.
///
/// `/hook/wait` is how an idle Claude Code session hears a direct message
/// without the live-channel flag: the `asyncRewake` hook `marshal-shim
/// claude-hook wait` (or `marshal-shim watch` under Claude's Monitor tool)
/// holds this request open, and what it returns wakes the session the way a
/// channel push would.
pub fn begin_wait(
    query: &str,
    body: &[u8],
    ctx: &Arc<MykoServerContext>,
) -> Option<(String, CommandContext)> {
    register_hook_session(query, body, ctx, Activity::Waiting)
}

/// How long a `WakeWatcher` counts without being refreshed, which is how long a
/// wait that timed out and wasn't followed by another still shows as waiting.
const WAKE_WATCHER_TTL_MS: i64 = 2 * 60 * 1000;
/// Refresh a held wait once this old, so its inbox checks don't each write an event.
const WAKE_WATCHER_REFRESH_MS: i64 = 60 * 1000;

/// Whether a watcher lasting until `until` is due a refresh at `now`.
fn wake_watcher_due(until: Option<i64>, now: i64) -> bool {
    until.is_none_or(|until| until - now <= WAKE_WATCHER_TTL_MS - WAKE_WATCHER_REFRESH_MS)
}

/// Record that this session is holding a wait. Called on every inbox check, so a
/// record removed when a resumed session's old waiter drops comes back within one
/// check, and a refresh is written only when due.
pub fn mark_waiting_now(cmd_ctx: &CommandContext, sid: &str) {
    mark_waiting(cmd_ctx, sid, chrono::Utc::now().timestamp_millis());
}

fn mark_waiting(cmd_ctx: &CommandContext, sid: &str, now: i64) {
    let watchers: Vec<Arc<WakeWatcher>> = cmd_ctx
        .exec_query(GetAllWakeWatchers {})
        .unwrap_or_default();
    let until = watchers
        .iter()
        .find(|w| w.id.0.as_ref() == sid)
        .map(|w| w.until);
    if !wake_watcher_due(until, now) {
        return;
    }
    let watcher = WakeWatcher {
        id: WakeWatcherId(Arc::from(sid)),
        until: now + WAKE_WATCHER_TTL_MS,
    };
    if let Err(e) = cmd_ctx.emit_set(&watcher) {
        log::warn!("[hook] wake watcher SET failed for {sid}: {e:?}");
    }
}

/// Tells the reader how to fetch the rest. The URI has to be one the resource
/// actually serves: a message id isn't a path under `marshal://messages`.
fn truncation_notice(message_id: &str, sent_at: i64) -> String {
    format!(
        "  [truncated; read message {message_id} in full at marshal://messages?inbox=true&since={sent_at}]\n"
    )
}

/// A wait ended with its connection dropped or its mail handed out, so stop
/// reporting it. Any other waiter for the session puts the record back at its next
/// inbox check. A wait that merely timed out keeps its record, because the waiter
/// normally asks again at once.
pub fn end_wait(cmd_ctx: &CommandContext, sid: &str) {
    let watcher = WakeWatcher {
        id: WakeWatcherId(Arc::from(sid)),
        until: 0,
    };
    if let Err(e) = cmd_ctx.emit_del(&watcher) {
        log::warn!("[hook] wake watcher DEL failed for {sid}: {e:?}");
    }
}

/// One inbox check for a `/hook/wait` long-poll. `None` while nothing is
/// unread; otherwise the same `<marshal_inbox>` block the prompt hook would
/// surface, with its ack deferred until the response is written.
pub fn poll_wait(cmd_ctx: &CommandContext, sid: &str) -> Option<HookOutcome> {
    let (inbox, ids) = surface_unread(cmd_ctx, sid);
    (!ids.is_empty()).then(|| HookOutcome {
        body: inbox,
        deferred_ack: Some((SessionId(Arc::from(sid)), ids)),
    })
}

fn handle_session_end(body: &[u8], ctx: &Arc<MykoServerContext>) -> HookOutcome {
    let Some(body) = parse_body(body) else {
        return HookOutcome::text(String::new());
    };
    let Some(sid) = body.get("session_id").and_then(|v| v.as_str()) else {
        return HookOutcome::text(String::new());
    };
    let cmd_ctx = internal_cmd_ctx(ctx);
    let stub = Session {
        id: SessionId(Arc::from(sid)),
        client_id: None,
        pid: 0,
        cwd: String::new(),
        git_branch: None,
        current_task: None,
        session_name: None,
        activity: None,
        kind: None,
        connected_at: 0,
        last_activity_at: None,
        last_tool: None,
        last_tool_at: None,
        operator: None,
        host: None,
        project: None,
        channels_enabled: None,
    };
    if let Err(e) = cmd_ctx.emit_del(&stub) {
        log::warn!("[hook] session-end DEL failed for {sid}: {e:?}");
    }
    end_wait(&cmd_ctx, sid);
    HookOutcome::text(String::new())
}

/// Fetch unread messages addressed to `sid`, format them framed as
/// untrusted context, ack them, and return the text. Empty string when
/// there's nothing — curl then prints nothing and no context is added.
fn surface_unread(cmd_ctx: &CommandContext, sid: &str) -> (String, Vec<MessageId>) {
    let sid_typed = SessionId(Arc::from(sid));
    // DIRECT-ONLY auto-inject. The per-turn inbox surfaces messages addressed
    // to me *directly* (`to_session`), NOT room broadcasts — a broadcast is
    // ambient (read via `marshal://messages room=…` or the marshal UI), so it
    // never hijacks the turn with unrelated context. `inbox: true` (direct +
    // room) stays available for explicit reads; auto-inject is direct-only.
    let read = ReadMessages {
        room: None,
        from: None,
        to_session: Some(sid_typed.clone()),
        inbox: false,
        sent: false,
        unread: true,
        since: None,
        limit: Some(20),
        as_session: Some(sid_typed.clone()),
    };
    let mut result = match read.execute(cmd_ctx.clone()) {
        Ok(r) => r,
        Err(_) => return (String::new(), Vec::new()),
    };
    // Mail another reader is delivering isn't offered twice.
    let unread: Vec<MessageId> = result
        .messages
        .iter()
        .map(|m| m.message_id.clone())
        .collect();
    let Ok(mut leases) = LEASES.lock() else {
        return (String::new(), Vec::new());
    };
    let lease = lease_duration();
    let taken = leases.take(&sid_typed, &unread, Instant::now(), lease);
    drop(leases);
    if taken.reoffered > 0 {
        log::info!(
            "[hook] {}: offering {} message(s) again: the last reader didn't ack within the {} s lease",
            sid_typed.0.as_ref(),
            taken.reoffered,
            lease.as_secs()
        );
    }
    result
        .messages
        .retain(|m| taken.ids.contains(&m.message_id));
    if result.messages.is_empty() {
        return (String::new(), Vec::new());
    }

    // Automatic injection is deliberately bounded. The durable Message keeps
    // the complete body; a hook carries only a UTF-8-safe preview so one log
    // dump cannot monopolize the recipient's model context. Divide an 8k body
    // budget across this batch, never exceeding the per-message live-push cap.
    let per_message_chars = (AUTO_INBOX_BODY_BUDGET_CHARS / result.messages.len())
        .clamp(AUTO_INBOX_MIN_BODY_CHARS, CONTEXT_BODY_MAX_CHARS);
    let render_line = |m: &MessageView| -> String {
        let sender = nickname_for(cmd_ctx, m.from_session_id.0.as_ref())
            .unwrap_or_else(|_| marshal_entities::nickname(m.from_session_id.0.as_ref()));
        let (preview, truncated) = context_preview(&m.body, per_message_chars);
        let mut line = format!(
            "- from {sender} [response_required={}]: {preview}\n",
            m.response_required
        );
        if truncated {
            line.push_str(&truncation_notice(m.message_id.0.as_ref(), m.sent_at));
        }
        line
    };

    // Partition the inbox: messages addressed to the OPERATOR (a human, via
    // their operator identity — routed here because this agent is that
    // operator's most-active session) vs ordinary agent-to-agent mail. Human
    // mail is surfaced FIRST and under a relay-to-your-operator contract — the
    // agent's job is to put it in front of the person, not to act on it. Agent
    // mail keeps the untrusted-peer stance. Without this split a message meant
    // for a human lands in an agent's lap framed as "untrusted, don't act,"
    // where nothing carries it to the person it was for.
    let (human, agent): (Vec<&MessageView>, Vec<&MessageView>) = result
        .messages
        .iter()
        .partition(|m| m.to_operator.is_some());

    let mut out = String::new();
    let remaining = result
        .total_matched
        .saturating_sub(result.messages.len() as u32);
    if remaining == 0 {
        out.push_str(&format!(
            "<marshal_inbox count=\"{}\">\n",
            result.messages.len()
        ));
    } else {
        out.push_str(&format!(
            "<marshal_inbox count=\"{}\" remaining=\"{remaining}\">\n",
            result.messages.len()
        ));
    }
    if !human.is_empty() {
        let op = human[0].to_operator.as_deref().unwrap_or("your operator");
        out.push_str(&marshal_entities::operator_relay_notice(op));
        out.push('\n');
        for m in &human {
            out.push_str(&render_line(m));
        }
    }
    if !agent.is_empty() {
        out.push_str(marshal_entities::PEER_MESSAGE_NOTICE);
        out.push('\n');
        for m in &agent {
            out.push_str(&render_line(m));
        }
    }
    out.push_str("</marshal_inbox>\n");

    // Return the surfaced ids for the listener to ack AFTER the response is
    // written (see `HookOutcome` / `ack_surfaced`). Acking here — before the
    // `<marshal_inbox>` bytes reach the agent — would lose messages on a
    // dropped or timed-out response.
    let ids: Vec<MessageId> = result
        .messages
        .iter()
        .map(|m| m.message_id.clone())
        .collect();

    (out, ids)
}

/// Build an internal (clientless) `CommandContext`. Commands run through
/// it carry no WS `client_id`, so they must self-identify via `asSession`.
fn internal_cmd_ctx(ctx: &Arc<MykoServerContext>) -> CommandContext {
    let tx: Arc<str> = uuid::Uuid::new_v4().to_string().into();
    let req = RequestContext::internal(tx, ctx.host_id, "hook");
    CommandContext::new(Arc::from("hook"), Arc::new(req), ctx.clone())
}

fn parse_body(body: &[u8]) -> Option<Value> {
    serde_json::from_slice(body).ok()
}

/// Parse a `k=v&k2=v2` query string with minimal percent/`+` decoding.
fn parse_query(qs: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for pair in qs.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        out.insert(k.to_string(), url_decode(v));
    }
    out
}

fn url_decode(s: &str) -> String {
    if !s.contains('%') && !s.contains('+') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut bytes = s.bytes();
    while let Some(b) = bytes.next() {
        match b {
            b'+' => out.push(' '),
            b'%' => {
                let h1 = bytes.next();
                let h2 = bytes.next();
                if let (Some(h1), Some(h2)) = (h1, h2)
                    && let (Some(d1), Some(d2)) =
                        ((h1 as char).to_digit(16), (h2 as char).to_digit(16))
                {
                    out.push(((d1 * 16 + d2) as u8) as char);
                    continue;
                }
                out.push('%');
            }
            _ => out.push(b as char),
        }
    }
    out
}

#[cfg(test)]
mod lease_tests {
    use super::*;

    fn id(s: &str) -> MessageId {
        MessageId(Arc::from(s))
    }

    #[test]
    fn a_second_reader_gets_nothing_until_the_lease_lapses() {
        let session = SessionId(Arc::from("s"));
        let ids = [id("m")];
        let lease = Duration::from_secs(60);
        let now = Instant::now();
        let mut leases = Leases::default();
        let first = leases.take(&session, &ids, now, lease);
        assert_eq!((first.ids.len(), first.reoffered), (1, 0));
        assert!(leases.take(&session, &ids, now, lease).ids.is_empty());
        // No ack ever came: the path was lost, so offer it again, and say so.
        let again = leases.take(&session, &ids, now + lease, lease);
        assert_eq!((again.ids.len(), again.reoffered), (1, 1));
    }

    #[test]
    fn a_lapsed_lease_is_remembered_for_the_log_but_not_for_ever() {
        let session = SessionId(Arc::from("s"));
        let (m, other) = ([id("m")], [id("other")]);
        let lease = Duration::from_secs(60);
        let now = Instant::now();
        let mut leases = Leases::default();
        leases.take(&session, &m, now, lease);
        // Another reader's take after m's lease lapsed doesn't forget it.
        leases.take(&session, &other, now + lease * 2, lease);
        assert_eq!(
            leases.take(&session, &m, now + lease * 3, lease).reoffered,
            1
        );
        // An hour after it lapsed, it's forgotten: a first offer again.
        let later = now + lease * 4 + FORGET_LAPSED;
        assert_eq!(leases.take(&session, &m, later, lease).reoffered, 0);
    }

    #[test]
    fn an_acked_message_offered_again_is_not_a_reoffer() {
        let session = SessionId(Arc::from("s"));
        let ids = [id("m")];
        let lease = Duration::from_secs(60);
        let now = Instant::now();
        let mut leases = Leases::default();
        leases.take(&session, &ids, now, lease);
        leases.release(&session, &ids);
        assert_eq!(leases.take(&session, &ids, now + lease, lease).reoffered, 0);
    }

    #[test]
    fn release_ends_a_lease_and_readers_of_other_sessions_are_unaffected() {
        let (a, b) = (SessionId(Arc::from("a")), SessionId(Arc::from("b")));
        let ids = [id("m")];
        let lease = Duration::from_secs(60);
        let now = Instant::now();
        let mut leases = Leases::default();
        leases.take(&a, &ids, now, lease);
        assert_eq!(leases.take(&b, &ids, now, lease).ids.len(), 1);
        leases.release(&a, &ids);
        assert_eq!(leases.take(&a, &ids, now, lease).ids.len(), 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_truncation_notice_names_a_uri_the_resource_serves() {
        let notice = truncation_notice("abc", 123);
        assert!(notice.contains("message abc"));
        assert!(notice.contains("marshal://messages?inbox=true&since=123"));
    }

    #[test]
    fn a_wake_watcher_is_refreshed_only_when_due() {
        assert!(wake_watcher_due(None, 1000));
        let fresh = 1000 + WAKE_WATCHER_TTL_MS;
        assert!(!wake_watcher_due(Some(fresh), 1000));
        assert!(!wake_watcher_due(
            Some(fresh),
            1000 + WAKE_WATCHER_REFRESH_MS - 1
        ));
        assert!(wake_watcher_due(
            Some(fresh),
            1000 + WAKE_WATCHER_REFRESH_MS
        ));
    }
}
