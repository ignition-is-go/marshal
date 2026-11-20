//! Resolve the server address from existing Marshal configuration.

pub const DEFAULT_DAEMON_ADDRESS: &str = "ws://localhost:6155";

/// Env var that overrides the daemon WebSocket URL. Set this to point
/// the shim at a daemon other than the default `ws://localhost:6155` —
/// e.g. a daemon on another host or a non-default port. The plugin's
/// `.mcp.json` plumbs this through with `${MARSHAL_DAEMON_ADDRESS}`
/// substitution, so a user-shell `export` is enough to reach Claude
/// Code's spawned shim. The legacy `MYKO_ADDRESS` name is honored as
/// a fallback so existing setups don't break on upgrade.
pub const ADDRESS_ENV: &str = "MARSHAL_DAEMON_ADDRESS";
pub const ADDRESS_ENV_LEGACY: &str = "MYKO_ADDRESS";

pub const ADDRESS_FILE: &str = "daemon-address";

/// The daemon this shim connects to, in the order described in `main`:
/// address file, then `MARSHAL_DAEMON_ADDRESS`, then `MYKO_ADDRESS`, then
/// localhost. The Claude/Codex hook bridge derives its hook URL from the same
/// answer.
pub fn server_address() -> String {
    first_daemon_address(
        read_address_from_config_file(),
        std::env::var(ADDRESS_ENV).ok(),
        std::env::var(ADDRESS_ENV_LEGACY).ok(),
    )
}

fn first_daemon_address(
    file: Option<String>,
    env: Option<String>,
    legacy_env: Option<String>,
) -> String {
    file.or(env)
        .or(legacy_env)
        .unwrap_or_else(|| DEFAULT_DAEMON_ADDRESS.to_string())
}

pub fn read_address_from_config_file() -> Option<String> {
    for path in config_file_candidates(ADDRESS_FILE) {
        if let Ok(contents) = std::fs::read_to_string(&path) {
            let line = contents.lines().next().unwrap_or("").trim();
            if !line.is_empty() {
                log::info!(
                    "[marshal-client] daemon address from config file {}: {}",
                    path.display(),
                    line
                );
                return Some(line.to_string());
            }
        }
    }
    None
}

#[cfg(unix)]
pub fn config_file_candidates(filename: &str) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
        out.push(std::path::PathBuf::from(xdg).join("marshal").join(filename));
    }
    if let Some(home) = std::env::var_os("HOME") {
        out.push(
            std::path::PathBuf::from(home)
                .join(".config")
                .join("marshal")
                .join(filename),
        );
    }
    out
}

#[cfg(windows)]
pub fn config_file_candidates(filename: &str) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Some(appdata) = std::env::var_os("APPDATA") {
        out.push(
            std::path::PathBuf::from(appdata)
                .join("marshal")
                .join(filename),
        );
    }
    if let Some(pd) = std::env::var_os("PROGRAMDATA") {
        out.push(std::path::PathBuf::from(pd).join("marshal").join(filename));
    }
    out
}

#[cfg(not(any(unix, windows)))]
pub fn config_file_candidates(_filename: &str) -> Vec<std::path::PathBuf> {
    Vec::new()
}

/// The per-session config filename the shim mirrors its daemon-ASSIGNED
/// nickname into.
fn nickname_file_name(session_id: &str) -> String {
    format!("nickname-{session_id}")
}

/// Read the daemon-assigned nickname the shim mirrored for `session_id` (first
/// existing config-dir candidate). `None` → the caller (statusline) falls back
/// to the deterministic `marshal_entities::nickname`. This is why the
/// statusline shows the SAME handle peers address even when the daemon salted
/// this session's nickname on a collision.
pub fn read_assigned_nickname(session_id: &str) -> Option<String> {
    for path in config_file_candidates(&nickname_file_name(session_id)) {
        if let Ok(contents) = std::fs::read_to_string(&path) {
            let line = contents.lines().next().unwrap_or("").trim();
            if !line.is_empty() {
                return Some(line.to_string());
            }
        }
    }
    None
}

/// Mirror `nickname` for `session_id` to the first writable config-dir
/// candidate. Best-effort: a failure just leaves the statusline on its
/// deterministic fallback.
pub fn write_assigned_nickname(session_id: &str, nickname: &str) {
    let Some(path) = config_file_candidates(&nickname_file_name(session_id))
        .into_iter()
        .next()
    else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&path, nickname);
}

/// Apply a bearer token supplied by the operator to native clients.
pub fn configure_operator_token(client: &crate::MarshalClient) -> anyhow::Result<()> {
    let Some(path) = std::env::var_os("MARSHAL_OPERATOR_TOKEN_FILE") else {
        return Ok(());
    };
    let token = std::fs::read_to_string(path)?;
    anyhow::ensure!(!token.trim().is_empty(), "operator token file is empty");
    client
        .set_bearer_token(token.trim().to_owned())
        .map_err(anyhow::Error::msg)
}

/// Identify the Git origin without retaining its credentials.
pub fn repository_key(cwd: &str) -> Option<String> {
    if cwd.is_empty() {
        return None;
    }
    let output = std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    project_from_remote(std::str::from_utf8(&output.stdout).ok()?.trim())
}

pub fn project_from_remote(remote: &str) -> Option<String> {
    let (host, path) = if remote.contains("://") {
        let url = url::Url::parse(remote).ok()?;
        let host = url.host_str()?;
        let host = url
            .port()
            .map_or_else(|| host.to_owned(), |port| format!("{host}:{port}"));
        (host, url.path().to_owned())
    } else {
        // Git's SCP-style remote. A slash before the colon denotes a local path.
        let (authority, path) = remote.split_once(':')?;
        let host = authority.rsplit('@').next()?;
        if host.is_empty() || host.contains('/') || host.contains('\\') || host.len() == 1 {
            return None;
        }
        (host.to_ascii_lowercase(), path.to_owned())
    };
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    (!path.is_empty()).then(|| format!("{host}/{path}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_address_file_outranks_the_env_vars() {
        let some = |s: &str| Some(s.to_string());
        assert_eq!(
            first_daemon_address(
                some("ws://file:6155"),
                some("ws://env:6155"),
                some("ws://old:6155")
            ),
            "ws://file:6155"
        );
        assert_eq!(
            first_daemon_address(None, some("ws://env:6155"), some("ws://old:6155")),
            "ws://env:6155"
        );
        assert_eq!(
            first_daemon_address(None, None, some("ws://old:6155")),
            "ws://old:6155"
        );
        assert_eq!(
            first_daemon_address(None, None, None),
            DEFAULT_DAEMON_ADDRESS
        );
    }
}
