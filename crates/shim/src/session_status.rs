//! Per-claude-session ephemeral status file.
//!
//! Written by the MCP server (`crates/shim/src/mcp.rs`) when the client
//! initialize handshake reveals what capabilities Claude Code granted.
//! Read by the `statusline` subcommand (`crates/shim/src/statusline.rs`)
//! so the human-visible Claude Code status bar can flag degraded modes
//! at a glance — most importantly: a session launched without
//! `--dangerously-load-development-channels server:marshal`, where live
//! mid-turn push is silently dropped.
//!
//! Keyed by the shim's parent PID (= the claude.exe spawning both this
//! MCP server and the per-render statusline subprocess). Siblings on the
//! same host write to distinct files. The OS temp dir is the storage
//! site — these records are ephemeral per-session and cleaned up at
//! shim shutdown; their absence is harmless (statusline degrades to
//! "no extra info" rather than guessing).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStatus {
    /// `true` if Claude Code declared the experimental `claude/channel`
    /// capability in its `initialize` request — i.e. was launched with
    /// `--dangerously-load-development-channels server:marshal`. When
    /// `false`, every `notifications/claude/channel` push from this
    /// shim is silently dropped on the Claude side, so the operator
    /// gets no mid-turn delivery; peer messages only surface in
    /// `<marshal_inbox>` at the start of the next user turn.
    pub channel_granted: bool,
}

pub fn status_path(claude_pid: u32) -> PathBuf {
    std::env::temp_dir().join(format!("marshal-shim-status-{claude_pid}.json"))
}

pub fn write(claude_pid: u32, status: &SessionStatus) {
    let Ok(json) = serde_json::to_string(status) else {
        return;
    };
    let _ = std::fs::write(status_path(claude_pid), json);
}

pub fn read(claude_pid: u32) -> Option<SessionStatus> {
    let bytes = std::fs::read(status_path(claude_pid)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg_attr(not(test), allow(dead_code))]
pub fn remove(claude_pid: u32) {
    let _ = std::fs::remove_file(status_path(claude_pid));
}

/// Resolve the calling process's parent PID. Cross-platform. Returns
/// `None` only on truly unusual platforms or when the toolhelp snapshot
/// fails on Windows.
#[cfg(unix)]
pub fn parent_pid() -> Option<u32> {
    Some(unsafe { libc::getppid() } as u32)
}

#[cfg(windows)]
pub fn parent_pid() -> Option<u32> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32, Process32First, Process32Next,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut entry: PROCESSENTRY32 = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32>() as u32;
        let mut found = None;
        let my_pid = GetCurrentProcessId();
        if Process32First(snap, &mut entry) != 0 {
            loop {
                if entry.th32ProcessID == my_pid {
                    found = Some(entry.th32ParentProcessID);
                    break;
                }
                if Process32Next(snap, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
        found
    }
}

#[cfg(not(any(unix, windows)))]
pub fn parent_pid() -> Option<u32> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_then_read_round_trips() {
        let claude_pid = 999_999_001;
        // Cleanup any leftover from prior runs.
        remove(claude_pid);
        write(claude_pid, &SessionStatus { channel_granted: true });
        let got = read(claude_pid).unwrap();
        assert!(got.channel_granted);
        remove(claude_pid);

        write(
            claude_pid,
            &SessionStatus {
                channel_granted: false,
            },
        );
        let got = read(claude_pid).unwrap();
        assert!(!got.channel_granted);
        remove(claude_pid);
    }

    #[test]
    fn read_missing_returns_none() {
        let claude_pid = 999_999_002;
        remove(claude_pid);
        assert!(read(claude_pid).is_none());
    }
}
