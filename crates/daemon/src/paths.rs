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

    struct EnvGuard { key: &'static str, prev: Option<std::ffi::OsString> }
    impl EnvGuard {
        fn set(key: &'static str, val: &str) -> Self {
            let prev = std::env::var_os(key);
            std::env::set_var(key, val);
            Self { key, prev }
        }
        fn unset(key: &'static str) -> Self {
            let prev = std::env::var_os(key);
            std::env::remove_var(key);
            Self { key, prev }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    // NOTE: these tests mutate process env; rely on --test-threads=1 (Cargo.toml does not
    // configure that automatically — invoke with `cargo test -p daemon -- --test-threads=1`).
    // EnvGuard restores prior values on drop so tests don't leak state to siblings.

    #[test]
    fn state_dir_uses_xdg_state_home() {
        let _g1 = EnvGuard::set("XDG_STATE_HOME", "/tmp/xdg-state");
        let _g2 = EnvGuard::unset("HOME");
        assert_eq!(state_dir(), PathBuf::from("/tmp/xdg-state/claude-coord"));
    }

    #[test]
    fn state_dir_falls_back_to_home() {
        let _g1 = EnvGuard::unset("XDG_STATE_HOME");
        let _g2 = EnvGuard::set("HOME", "/home/test");
        assert_eq!(state_dir(), PathBuf::from("/home/test/.local/state/claude-coord"));
    }

    #[test]
    fn state_dir_last_resort_tmp() {
        let _g1 = EnvGuard::unset("XDG_STATE_HOME");
        let _g2 = EnvGuard::unset("HOME");
        assert_eq!(state_dir(), PathBuf::from("/tmp/claude-coord"));
    }

    #[test]
    fn socket_path_uses_xdg_runtime_dir() {
        let _g = EnvGuard::set("XDG_RUNTIME_DIR", "/run/user/1000");
        assert_eq!(socket_path(), PathBuf::from("/run/user/1000/claude-coord/sock"));
    }

    #[test]
    fn socket_path_falls_back_to_state_dir() {
        let _g1 = EnvGuard::unset("XDG_RUNTIME_DIR");
        let _g2 = EnvGuard::unset("XDG_STATE_HOME");
        let _g3 = EnvGuard::set("HOME", "/home/test");
        assert_eq!(socket_path(), PathBuf::from("/home/test/.local/state/claude-coord/sock"));
    }
}
