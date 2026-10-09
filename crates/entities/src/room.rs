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
    /// the user-supplied label (which `id` is slugified from). Follows the
    /// room text rule (`is_forbidden_room_text_char`, `ROOM_NAME_MAX_CHARS`)
    /// for rooms created since that rule; older stored rooms are not migrated.
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

// ─── Room text rule ────────────────────────────────────────────────────

/// Longest room name, in characters (Unicode scalar values, not bytes).
///
/// A room name is a short label: it is the body of the `@mention` live push
/// ("<nick> mentioned you in <room>"), a column in `marshal://rooms`, and a
/// row in the UI. Nothing documented a limit before, and the names in use are
/// a few words (`auth-refactor`, `Release prep`). 64 leaves room for a
/// descriptive label plus an auto-room prefix (`project:` + a long repo
/// basename) while keeping a hostile name from filling a model's context.
pub const ROOM_NAME_MAX_CHARS: usize = 64;

/// Longest room description, in characters. A description is a one- or
/// two-sentence purpose shown in `marshal://rooms`; 256 fits that with margin.
pub const ROOM_DESCRIPTION_MAX_CHARS: usize = 256;

/// Whether `ch` may not appear in a room name or description.
///
/// Room names and descriptions reach other sessions' model context verbatim
/// (the `@mention` channel push, the `marshal://rooms` resource), so this is
/// the one place that decides which characters could pass for framing there.
/// It is a blocklist, not an allowlist: markup and quoting characters, every
/// `char::is_control()` character, and a fixed list of the invisible and
/// formatting characters known to hide text. Characters it does not list are
/// allowed, so it makes no claim about invisible or confusable characters in
/// general. [`FORBIDDEN_RANGES`] lists every code point.
pub fn is_forbidden_room_text_char(ch: char) -> bool {
    forbidden_class(ch).is_some()
}

/// The blocked code points beyond `char::is_control()`, as inclusive ranges
/// with the class name the error message uses. Hand-written so the list is
/// exactly what is reviewed here; no Unicode-property dependency.
///
/// U+FE0F (variation selector 16) is deliberately absent: it makes ordinary
/// emoji such as ❤️ render in colour.
pub const FORBIDDEN_RANGES: &[(char, char, &str)] = &[
    // Markup and quoting, plus characters that render like `<` and `>`.
    ('<', '<', "an angle bracket"),
    ('>', '>', "an angle bracket"),
    ('\u{02C2}', '\u{02C3}', "an angle bracket"),
    ('\u{2039}', '\u{203A}', "an angle bracket"),
    ('\u{1433}', '\u{1433}', "an angle bracket"),
    ('\u{1438}', '\u{1438}', "an angle bracket"),
    ('\u{2329}', '\u{232A}', "an angle bracket"),
    ('\u{276C}', '\u{2771}', "an angle bracket"),
    ('\u{27E8}', '\u{27E9}', "an angle bracket"),
    ('\u{29FC}', '\u{29FD}', "an angle bracket"),
    ('\u{3008}', '\u{3009}', "an angle bracket"),
    ('\u{FE3F}', '\u{FE40}', "an angle bracket"),
    ('\u{FE64}', '\u{FE65}', "an angle bracket"),
    ('\u{FF1C}', '\u{FF1C}', "an angle bracket"),
    ('\u{FF1E}', '\u{FF1E}', "an angle bracket"),
    ('&', '&', "an ampersand"),
    ('"', '"', "a double quote"),
    ('`', '`', "a backtick"),
    // Line breaks that `is_control()` does not cover.
    ('\u{2028}', '\u{2029}', "a line or paragraph separator"),
    // Bidi controls.
    ('\u{061C}', '\u{061C}', "a bidi control"),
    ('\u{200E}', '\u{200F}', "a bidi control"),
    ('\u{202A}', '\u{202E}', "a bidi control"),
    ('\u{2066}', '\u{2069}', "a bidi control"),
    // Zero-width and other invisible characters.
    (
        '\u{00AD}',
        '\u{00AD}',
        "a zero-width or invisible character",
    ),
    (
        '\u{180E}',
        '\u{180E}',
        "a zero-width or invisible character",
    ),
    (
        '\u{200B}',
        '\u{200D}',
        "a zero-width or invisible character",
    ),
    (
        '\u{2060}',
        '\u{2064}',
        "a zero-width or invisible character",
    ),
    (
        '\u{FEFF}',
        '\u{FEFF}',
        "a zero-width or invisible character",
    ),
    ('\u{034F}', '\u{034F}', "a combining grapheme joiner"),
    ('\u{115F}', '\u{1160}', "a Hangul filler"),
    ('\u{3164}', '\u{3164}', "a Hangul filler"),
    ('\u{FFA0}', '\u{FFA0}', "a Hangul filler"),
    ('\u{17B4}', '\u{17B5}', "a Khmer inherent vowel"),
    ('\u{2800}', '\u{2800}', "a blank braille pattern"),
    // Variation selectors (VS16, U+FE0F, stays allowed; see above).
    ('\u{FE00}', '\u{FE0E}', "a variation selector"),
    ('\u{E0100}', '\u{E01EF}', "a variation selector"),
    (
        '\u{180B}',
        '\u{180D}',
        "a Mongolian free variation selector",
    ),
    (
        '\u{180F}',
        '\u{180F}',
        "a Mongolian free variation selector",
    ),
    // Formatting controls.
    ('\u{206A}', '\u{206F}', "a deprecated formatting control"),
    ('\u{FFF9}', '\u{FFFB}', "an interlinear annotation control"),
    (
        '\u{13430}',
        '\u{1343F}',
        "an Egyptian hieroglyph format control",
    ),
    ('\u{1BCA0}', '\u{1BCA3}', "a shorthand format control"),
    ('\u{1D173}', '\u{1D17A}', "a musical format control"),
    // Tag characters can spell hidden ASCII.
    ('\u{E0000}', '\u{E007F}', "a tag character"),
];

/// The class `ch` belongs to when it is forbidden, worded for an error
/// message without quoting the character itself.
fn forbidden_class(ch: char) -> Option<&'static str> {
    if let Some((_, _, class)) = FORBIDDEN_RANGES
        .iter()
        .find(|(low, high, _)| (*low..=*high).contains(&ch))
    {
        return Some(class);
    }
    ch.is_control()
        .then_some("a control character (line breaks and tabs included)")
}

/// Check a user-supplied room name against the room text rule and
/// [`ROOM_NAME_MAX_CHARS`]. The error names the offending character by code
/// point and position, and never echoes the input.
pub fn validate_room_name(name: &str) -> Result<(), String> {
    validate_room_text("room name", name, ROOM_NAME_MAX_CHARS)
}

/// Check a user-supplied room description against the room text rule and
/// [`ROOM_DESCRIPTION_MAX_CHARS`].
pub fn validate_room_description(description: &str) -> Result<(), String> {
    validate_room_text("room description", description, ROOM_DESCRIPTION_MAX_CHARS)
}

fn validate_room_text(field: &str, text: &str, max_chars: usize) -> Result<(), String> {
    if let Some((index, ch, class)) = text
        .chars()
        .enumerate()
        .find_map(|(i, c)| forbidden_class(c).map(|class| (i, c, class)))
    {
        return Err(format!(
            "{field} contains {class} (U+{:04X}) at character {}; room names and \
             descriptions may not contain angle brackets (and the listed lookalikes), ampersands, \
             double quotes, backticks, control characters, line separators, or \
             characters on a blocklist of the invisible and formatting characters \
             known to hide text",
            ch as u32,
            index + 1,
        ));
    }
    let count = text.chars().count();
    if count > max_chars {
        return Err(format!(
            "{field} is {count} characters; the limit is {max_chars}"
        ));
    }
    Ok(())
}

/// Make a room name from text the daemon derives rather than asks for:
/// drop every forbidden character and keep at most [`ROOM_NAME_MAX_CHARS`]
/// characters. Never fails; the result can be empty.
pub fn sanitize_room_name(raw: &str) -> String {
    sanitize_room_text(raw, ROOM_NAME_MAX_CHARS)
}

/// The description counterpart of [`sanitize_room_name`], capped at
/// [`ROOM_DESCRIPTION_MAX_CHARS`].
pub fn sanitize_room_description(raw: &str) -> String {
    sanitize_room_text(raw, ROOM_DESCRIPTION_MAX_CHARS)
}

/// Drop every forbidden character from `raw` and keep at most `max_chars`
/// characters (Unicode scalar values). For callers that compose a name from
/// parts, such as an auto-room prefix plus a value.
pub fn sanitize_room_text(raw: &str, max_chars: usize) -> String {
    raw.chars()
        .filter(|c| !is_forbidden_room_text_char(*c))
        .take(max_chars)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forbidden_ranges_are_inclusive_at_both_ends() {
        let cases: &[(&str, char, bool)] = &[
            ("bidi embedding start", '\u{202A}', true),
            ("bidi override end", '\u{202E}', true),
            ("after bidi overrides", '\u{202F}', false),
            ("isolate start", '\u{2066}', true),
            ("isolate end", '\u{2069}', true),
            ("zero-width start", '\u{200B}', true),
            ("zero-width end", '\u{200D}', true),
            ("invisible operators end", '\u{2064}', true),
            ("after invisible operators", '\u{2065}', false),
            ("tag block start", '\u{E0000}', true),
            ("tag block end", '\u{E007F}', true),
            ("after tag block", '\u{E0080}', false),
            ("variation selector 1", '\u{FE00}', true),
            ("variation selector 15", '\u{FE0E}', true),
            (
                "variation selector 16 stays allowed for emoji",
                '\u{FE0F}',
                false,
            ),
            ("supplementary variation selector start", '\u{E0100}', true),
            ("supplementary variation selector end", '\u{E01EF}', true),
            (
                "after supplementary variation selectors",
                '\u{E01F0}',
                false,
            ),
            ("combining grapheme joiner", '\u{034F}', true),
            ("hangul choseong filler", '\u{115F}', true),
            ("hangul jungseong filler", '\u{1160}', true),
            ("hangul filler", '\u{3164}', true),
            ("halfwidth hangul filler", '\u{FFA0}', true),
            ("interlinear annotation start", '\u{FFF9}', true),
            ("interlinear annotation end", '\u{FFFB}', true),
            ("after interlinear annotation", '\u{FFFC}', false),
            ("deprecated format controls start", '\u{206A}', true),
            ("deprecated format controls end", '\u{206F}', true),
            ("mongolian free variation selector start", '\u{180B}', true),
            ("mongolian free variation selector 3", '\u{180D}', true),
            ("mongolian free variation selector 4", '\u{180F}', true),
            ("mongolian digit zero", '\u{1810}', false),
            ("khmer inherent vowel aq", '\u{17B4}', true),
            ("khmer inherent vowel aa", '\u{17B5}', true),
            ("khmer vowel sign aa", '\u{17B6}', false),
            ("musical format start", '\u{1D173}', true),
            ("musical format end", '\u{1D17A}', true),
            ("after musical format", '\u{1D17B}', false),
            ("shorthand format start", '\u{1BCA0}', true),
            ("shorthand format end", '\u{1BCA3}', true),
            ("egyptian format start", '\u{13430}', true),
            ("egyptian format end", '\u{1343F}', true),
            ("after egyptian format", '\u{13440}', false),
            ("blank braille", '\u{2800}', true),
            ("braille dot 1", '\u{2801}', false),
            ("fullwidth less-than", '\u{FF1C}', true),
            ("fullwidth greater-than", '\u{FF1E}', true),
            ("small less-than", '\u{FE64}', true),
            ("small greater-than", '\u{FE65}', true),
            ("modifier arrowheads", '\u{02C2}', true),
            ("modifier arrowheads end", '\u{02C3}', true),
            ("single angle quotation marks", '\u{2039}', true),
            ("single angle quotation marks end", '\u{203A}', true),
            ("angle brackets block", '\u{2329}', true),
            ("angle brackets block end", '\u{232A}', true),
            ("cjk angle brackets", '\u{3008}', true),
            ("cjk angle brackets end", '\u{3009}', true),
            ("cjk double angle bracket", '\u{300A}', false),
            ("mathematical angle brackets", '\u{27E8}', true),
            ("mathematical angle brackets end", '\u{27E9}', true),
            ("guillemet", '\u{00AB}', false),
            ("before angle bracket ornaments", '\u{276B}', false),
            ("angle bracket ornaments start", '\u{276C}', true),
            ("angle bracket ornaments end", '\u{2771}', true),
            ("after angle bracket ornaments", '\u{2772}', false),
            ("curved angle brackets", '\u{29FC}', true),
            ("curved angle brackets end", '\u{29FD}', true),
            ("after curved angle brackets", '\u{29FE}', false),
            ("vertical angle bracket forms", '\u{FE3F}', true),
            ("vertical angle bracket forms end", '\u{FE40}', true),
            ("after vertical angle bracket forms", '\u{FE41}', false),
            ("canadian syllabics po", '\u{1433}', true),
            ("canadian syllabics between", '\u{1434}', false),
            ("canadian syllabics pa", '\u{1438}', true),
            ("plain space", ' ', false),
            ("apostrophe", '\'', false),
            ("accented letter", 'é', false),
        ];
        for (case, ch, forbidden) in cases {
            assert_eq!(is_forbidden_room_text_char(*ch), *forbidden, "{case}");
        }
    }

    #[test]
    fn sanitize_room_name_drops_forbidden_and_caps_characters() {
        assert_eq!(sanitize_room_name("project:a<b>\n"), "project:ab");
        let long = "é".repeat(ROOM_NAME_MAX_CHARS + 5);
        assert_eq!(
            sanitize_room_name(&long).chars().count(),
            ROOM_NAME_MAX_CHARS
        );
    }
}
