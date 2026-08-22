//! `Room` entity — named groupings of `Session`s.
//!
//! Routing scope, not a topic feed: messages addressed to a room get
//! fan-out delivery to every current `RoomMember`. See the
//! `BroadcastMessage` command for delivery semantics.
//!
//! Rooms come in two kinds:
//! - `Auto`: derived from a session's identity. The daemon's
//!   `AutoRoomSaga` keeps the anchor rooms (`everyone`, `op:*`,
//!   `project:*`) in sync as sessions arrive/leave. (`host:*` was
//!   retired — a per-node singleton with no coordination value; the
//!   `AutoSource::Host` variant is kept only so older persisted host
//!   rooms still deserialize until the cleanup GC reaps them.)
//! - `Adhoc`: anything a user creates with `join_room("anything")`.

use myko::myko_item;
use myko::prelude::{EventPublishing as _, Querying as _, RegistryScoped as _, RequestScoped as _};
use serde::{Deserialize, Serialize};

#[myko_item]
pub struct Room {
    /// Display name. For auto-rooms, equal to `id`. For ad-hoc rooms,
    /// the user-supplied label (which `id` is slugified from).
    #[myko_setter]
    pub name: String,

    /// Optional human-readable purpose for the room (set by the
    /// creator, displayed in `list_rooms`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[myko_setter]
    pub description: Option<String>,

    /// Whether this room is identity-anchored or user-created.
    pub kind: RoomKind,

    /// Wall-clock millis when the room was first created.
    pub created_at: i64,
}

/// What kind of room this is. Auto-rooms carry their `AutoSource` so
/// the saga can reconcile membership and the UI can flag them
/// differently from ad-hoc rooms (icon, dim color, can't-leave).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(myko::TS), ts(export))]
pub enum RoomKind {
    Auto {
        source: AutoSource,
    },
    #[default]
    Adhoc,
}

/// Identity attribute that anchors an auto-room. The daemon's
/// `AutoRoomSaga` recomputes membership whenever a `Session` SETs by
/// reading its `operator` / `host` / `cwd` and (de)joining the
/// matching auto-rooms.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "anchor", rename_all = "camelCase")]
#[cfg_attr(feature = "ts-export", derive(myko::TS), ts(export))]
pub enum AutoSource {
    /// Singleton — every live session is a member. Never DEL'd.
    Everyone,
    /// `host:<name>` — anchored on `Session.host.name`.
    Host { name: String },
    /// `op:<operator>` — anchored on `Session.operator`.
    Operator { name: String },
    /// `project:<basename>` — anchored on the basename of the git
    /// repo root containing the session's `cwd`.
    Project { basename: String },
}

myko::impl_filterable_opaque!(RoomKind, AutoSource);
