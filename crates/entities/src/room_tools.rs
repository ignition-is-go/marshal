//! Room membership tools — `JoinRoom` and `LeaveRoom` server commands.
//!
//! Both resolve the calling session from `ctx.client_id()` (same path
//! as `SendMessage` and `BroadcastMessage`), so the shim never has to
//! pass its own session id.
//!
//! Listing rooms is the auto-generated `GetAllRooms` query — clients
//! call it directly and join with `GetAllRoomMembers` for memberships;
//! no dedicated `list_rooms` command is needed.

use std::sync::Arc;

use chrono::Utc;
use myko::{
    command::{CommandContext, CommandError, CommandHandler},
    myko_command,
};
use serde::{Deserialize, Serialize};

use crate::{
    room::{GetAllRooms, Room, RoomId, RoomKind},
    room_member::{GetAllRoomMembers, RoomMember, RoomMemberId},
    session::{GetAllSessions, Session, SessionId, resolve_caller},
};

// ─── JoinRoom ──────────────────────────────────────────────────────────

#[myko_command(JoinRoomResult)]
pub struct JoinRoom {
    /// User-supplied display name. Slugified into the room id; rooms
    /// with reserved prefixes (`everyone`, `host:`, `op:`, `project:`)
    /// are blocked so users can't shadow auto-rooms.
    pub name: String,

    /// Optional human-readable purpose, surfaced in `list_rooms`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Self-identified caller for connectionless paths (HTTP-MCP agents).
    /// WS shim callers omit it. See `resolve_caller`.
    #[serde(default, rename = "asSession", skip_serializing_if = "Option::is_none")]
    pub as_session: Option<SessionId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JoinRoomResult {
    pub room_id: RoomId,
    pub name: String,
    /// `true` if this call created the membership row; `false` if the
    /// session was already a member (idempotent join).
    pub joined: bool,
    /// `true` if this call created the room; `false` if the room
    /// already existed and we just attached membership.
    pub created: bool,
}

impl CommandHandler for JoinRoom {
    #[cfg(target_arch = "wasm32")]
    fn execute(self, _ctx: CommandContext) -> Result<Self::Result, CommandError> {
        unreachable!("JoinRoom::execute is server-only");
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn execute(self, ctx: CommandContext) -> Result<Self::Result, CommandError> {
        if is_reserved_room_name(&self.name) {
            return Err(err(
                &ctx,
                &format!(
                    "'{}' is reserved for auto-rooms; pick a different name",
                    self.name,
                ),
            ));
        }
        let slug = slugify(&self.name);
        if slug.is_empty() {
            return Err(err(
                &ctx,
                "room name must contain at least one alphanumeric character",
            ));
        }

        let sender = resolve_caller_session(&ctx, self.as_session.as_ref())?;

        let rooms: Vec<Arc<Room>> = ctx.exec_query(GetAllRooms {})?;
        let memberships: Vec<Arc<RoomMember>> = ctx.exec_query(GetAllRoomMembers {})?;

        // Find or create the room. We slugify and append `-N` on
        // collision (mirrors the nickname dedup convention).
        let room_id_str = pick_unique_room_id(&slug, &rooms);
        let room_id = RoomId(Arc::from(room_id_str.as_str()));
        let now = Utc::now().timestamp_millis();

        let (room, created) = match rooms.iter().find(|r| r.id == room_id) {
            Some(existing) => ((**existing).clone(), false),
            None => {
                let room = Room {
                    id: room_id.clone(),
                    name: self.name.clone(),
                    description: self.description.clone(),
                    kind: RoomKind::Adhoc,
                    created_at: now,
                };
                ctx.emit_set(&room)?;
                (room, true)
            }
        };

        // Idempotent membership write.
        let member_id = RoomMember::make_id(room.id.0.as_ref(), sender.id.0.as_ref());
        let already_member = memberships
            .iter()
            .any(|m| m.id.0.as_ref() == member_id.as_str());
        let joined = !already_member;
        if joined {
            ctx.emit_set(&RoomMember {
                id: RoomMemberId(Arc::from(member_id.as_str())),
                room_id: room.id.clone(),
                session_id: sender.id.clone(),
                joined_at: now,
            })?;
        }

        Ok(JoinRoomResult {
            room_id: room.id.clone(),
            name: room.name.clone(),
            joined,
            created,
        })
    }
}

// ─── LeaveRoom ─────────────────────────────────────────────────────────

#[myko_command(LeaveRoomResult)]
pub struct LeaveRoom {
    /// Either a `room_id` (preferred) or the original room name —
    /// the server tries id-match first, then name-match.
    pub room: String,

    /// Self-identified caller for connectionless paths (HTTP-MCP agents).
    /// WS shim callers omit it. See `resolve_caller`.
    #[serde(default, rename = "asSession", skip_serializing_if = "Option::is_none")]
    pub as_session: Option<SessionId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaveRoomResult {
    pub room_id: RoomId,
    /// `true` if the call removed an existing membership; `false`
    /// if the session wasn't a member (idempotent leave).
    pub left: bool,
}

impl CommandHandler for LeaveRoom {
    #[cfg(target_arch = "wasm32")]
    fn execute(self, _ctx: CommandContext) -> Result<Self::Result, CommandError> {
        unreachable!("LeaveRoom::execute is server-only");
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn execute(self, ctx: CommandContext) -> Result<Self::Result, CommandError> {
        let sender = resolve_caller_session(&ctx, self.as_session.as_ref())?;
        let rooms: Vec<Arc<Room>> = ctx.exec_query(GetAllRooms {})?;
        let memberships: Vec<Arc<RoomMember>> = ctx.exec_query(GetAllRoomMembers {})?;

        let room = rooms
            .iter()
            .find(|r| r.id.0.as_ref() == self.room || r.name == self.room)
            .ok_or_else(|| err(&ctx, &format!("no room matching '{}'", self.room)))?
            .clone();

        // Auto-rooms aren't user-leavable — their membership is
        // recomputed by the AutoRoomSaga from the session's identity.
        if !matches!(room.kind, RoomKind::Adhoc) {
            return Err(err(
                &ctx,
                &format!(
                    "room '{}' is an auto-room ({:?}); membership is derived from your session's identity",
                    room.id.0.as_ref(),
                    room.kind,
                ),
            ));
        }

        let member_id = RoomMember::make_id(room.id.0.as_ref(), sender.id.0.as_ref());
        let existing = memberships
            .iter()
            .find(|m| m.id.0.as_ref() == member_id.as_str())
            .cloned();

        let left = if let Some(member) = existing {
            ctx.emit_del(&*member)?;
            true
        } else {
            false
        };

        Ok(LeaveRoomResult {
            room_id: room.id.clone(),
            left,
        })
    }
}

// ─── Helpers ───────────────────────────────────────────────────────────

fn err(ctx: &CommandContext, message: &str) -> CommandError {
    CommandError {
        tx: ctx.tx().to_string(),
        command_id: ctx.command_id.to_string(),
        message: message.to_string(),
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn resolve_caller_session(
    ctx: &CommandContext,
    as_session: Option<&SessionId>,
) -> Result<Arc<Session>, CommandError> {
    let sessions: Vec<Arc<Session>> = ctx.exec_query(GetAllSessions {})?;
    resolve_caller(ctx, &sessions, as_session)
}

/// Reserved room names + prefixes that can't be created via JoinRoom.
/// Owned by the AutoRoomSaga.
pub fn is_reserved_room_name(name: &str) -> bool {
    let n = name.trim();
    if n.eq_ignore_ascii_case("everyone") {
        return true;
    }
    let lower = n.to_ascii_lowercase();
    lower.starts_with("host:") || lower.starts_with("op:") || lower.starts_with("project:")
}

/// Slugify a user-supplied name into a room id: lowercase, ASCII
/// alphanumerics + `-`, runs of other chars collapse to `-`. Empty
/// result is checked by the caller and rejected.
pub fn slugify(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut prev_dash = true;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

fn pick_unique_room_id(slug: &str, rooms: &[Arc<Room>]) -> String {
    use std::collections::HashSet;
    let taken: HashSet<&str> = rooms.iter().map(|r| r.id.0.as_ref()).collect();
    if !taken.contains(slug) {
        return slug.to_string();
    }
    let mut n = 2u32;
    loop {
        let candidate = format!("{slug}-{n}");
        if !taken.contains(candidate.as_str()) {
            return candidate;
        }
        n += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_handles_ascii_and_runs() {
        assert_eq!(slugify("Frontend Redesign"), "frontend-redesign");
        assert_eq!(slugify("backend  ops"), "backend-ops");
        assert_eq!(slugify("ABC-123"), "abc-123");
        assert_eq!(slugify("--hello!!world--"), "hello-world");
        assert_eq!(slugify(""), "");
        assert_eq!(slugify("..."), "");
    }

    #[test]
    fn reserved_names_blocked_case_insensitive() {
        assert!(is_reserved_room_name("everyone"));
        assert!(is_reserved_room_name("EVERYONE"));
        assert!(is_reserved_room_name("host:laptop"));
        assert!(is_reserved_room_name("Op:trevor"));
        assert!(is_reserved_room_name("project:marshal"));
        assert!(!is_reserved_room_name("frontend"));
        assert!(!is_reserved_room_name("hostess"));
    }
}
