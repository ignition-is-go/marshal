//! Live roster of connected sessions and lookup helpers.

use proto::messages::SessionInfo;
use std::collections::HashMap;
use std::sync::Mutex;

pub struct Roster {
    inner: Mutex<HashMap<String, SessionInfo>>,
}

impl Roster {
    pub fn new() -> Self { Self { inner: Mutex::new(HashMap::new()) } }

    pub fn insert(&self, info: SessionInfo) {
        let mut g = self.inner.lock().unwrap();
        g.insert(info.session_id.clone(), info);
    }

    /// Insert only if the session_id is not already in the roster. Returns true on success.
    pub fn try_insert(&self, info: SessionInfo) -> bool {
        let mut g = self.inner.lock().unwrap();
        if g.contains_key(&info.session_id) { return false; }
        g.insert(info.session_id.clone(), info);
        true
    }

    /// Look up the nickname for a given session_id, if it is in the roster.
    pub fn nickname_of(&self, session_id: &str) -> Option<String> {
        let g = self.inner.lock().unwrap();
        g.get(session_id).map(|s| s.nickname.clone())
    }

    pub fn remove(&self, session_id: &str) {
        let mut g = self.inner.lock().unwrap();
        g.remove(session_id);
    }

    pub fn touch_heartbeat(&self, session_id: &str, now_ms: i64) {
        let mut g = self.inner.lock().unwrap();
        if let Some(s) = g.get_mut(session_id) {
            s.last_heartbeat = now_ms;
        }
    }

    pub fn set_status(&self, session_id: &str, text: String) {
        let mut g = self.inner.lock().unwrap();
        if let Some(s) = g.get_mut(session_id) {
            s.current_task = Some(text);
        }
    }

    /// Set the session's role. An empty string clears it.
    pub fn set_role(&self, session_id: &str, role: String) {
        let mut g = self.inner.lock().unwrap();
        if let Some(s) = g.get_mut(session_id) {
            s.role = if role.is_empty() { None } else { Some(role) };
        }
    }

    /// Snapshot of all sessions, with `is_self = true` on the row whose id matches `me`.
    pub fn snapshot(&self, me: &str) -> Vec<SessionInfo> {
        let g = self.inner.lock().unwrap();
        let mut out: Vec<SessionInfo> = g.values().cloned().collect();
        for s in &mut out { s.is_self = s.session_id == me; }
        out.sort_by(|a, b| a.nickname.cmp(&b.nickname));
        out
    }

    /// Resolve a "to" target: matches a session_id exactly, otherwise nickname.
    /// Returns `Ok((session_id, nickname))`.
    pub fn resolve(&self, target: &str) -> Result<(String, String), ResolveError> {
        let g = self.inner.lock().unwrap();
        if let Some(s) = g.get(target) {
            return Ok((s.session_id.clone(), s.nickname.clone()));
        }
        let matches: Vec<&SessionInfo> = g.values().filter(|s| s.nickname == target).collect();
        match matches.len() {
            0 => Err(ResolveError::Unknown(target.to_string())),
            1 => Ok((matches[0].session_id.clone(), matches[0].nickname.clone())),
            _ => Err(ResolveError::Ambiguous(
                matches.iter().map(|s| s.session_id.clone()).collect(),
            )),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("no live session matches '{0}'")]
    Unknown(String),
    #[error("multiple live sessions match: {0:?}")]
    Ambiguous(Vec<String>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn info(id: &str, nick: &str) -> SessionInfo {
        SessionInfo {
            session_id: id.into(),
            nickname: nick.into(),
            pid: 1,
            cwd: PathBuf::from("/x"),
            git_branch: None,
            current_task: None,
            role: None,
            connected_at: 0,
            last_heartbeat: 0,
            is_self: false,
        }
    }

    #[test]
    fn resolve_by_id() {
        let r = Roster::new();
        r.insert(info("s-1", "a"));
        let (id, nick) = r.resolve("s-1").unwrap();
        assert_eq!((id.as_str(), nick.as_str()), ("s-1", "a"));
    }

    #[test]
    fn resolve_by_nickname() {
        let r = Roster::new();
        r.insert(info("s-1", "eww"));
        let (id, _) = r.resolve("eww").unwrap();
        assert_eq!(id, "s-1");
    }

    #[test]
    fn resolve_unknown() {
        let r = Roster::new();
        assert!(matches!(r.resolve("ghost"), Err(ResolveError::Unknown(_))));
    }

    #[test]
    fn resolve_ambiguous() {
        let r = Roster::new();
        r.insert(info("s-1", "eww"));
        r.insert(info("s-2", "eww"));
        assert!(matches!(r.resolve("eww"), Err(ResolveError::Ambiguous(_))));
    }

    #[test]
    fn snapshot_marks_self() {
        let r = Roster::new();
        r.insert(info("s-1", "a"));
        r.insert(info("s-2", "b"));
        let snap = r.snapshot("s-2");
        let me: Vec<&SessionInfo> = snap.iter().filter(|s| s.is_self).collect();
        assert_eq!(me.len(), 1);
        assert_eq!(me[0].session_id, "s-2");
    }
}
