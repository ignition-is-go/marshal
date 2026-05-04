//! XDG-aware paths for socket, db, log.

use std::path::PathBuf;

/// State dir for the daemon. Order: $XDG_STATE_HOME/claude-coord, else ~/.local/state/claude-coord.
pub fn state_dir() -> PathBuf {
    if let Some(xs) = std::env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(xs).join("claude-coord");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".local/state/claude-coord");
    }
    PathBuf::from("/tmp/claude-coord")
}

/// Socket path. Order: $XDG_RUNTIME_DIR/claude-coord/sock, else state_dir()/sock.
pub fn socket_path() -> PathBuf {
    if let Some(rd) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(rd).join("claude-coord/sock");
    }
    state_dir().join("sock")
}

pub fn db_path() -> PathBuf { state_dir().join("db.sqlite") }
pub fn log_path() -> PathBuf { state_dir().join("daemon.log") }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_dir_uses_xdg_state_home() {
        // SAFETY: tests run single-threaded by default for env vars, but be defensive.
        let m = std::sync::Mutex::new(());
        let _g = m.lock().unwrap();
        std::env::set_var("XDG_STATE_HOME", "/tmp/xdg-state");
        std::env::remove_var("HOME");
        assert_eq!(state_dir(), PathBuf::from("/tmp/xdg-state/claude-coord"));
    }

    #[test]
    fn socket_path_uses_xdg_runtime_dir() {
        let m = std::sync::Mutex::new(());
        let _g = m.lock().unwrap();
        std::env::set_var("XDG_RUNTIME_DIR", "/run/user/1000");
        assert_eq!(socket_path(), PathBuf::from("/run/user/1000/claude-coord/sock"));
    }
}
