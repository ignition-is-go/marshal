//! Detect whether Claude Code granted the channel capability for marshal.
//!
//! Claude's channel-grant decision is purely client-side; it is **not**
//! communicated to the server over the MCP wire (cf. claude.exe's
//! `V7$` function which silently returns `{action:"skip"}` and never
//! tells the server). The only reliable signal the shim can read is
//! Claude's own argv: was it launched with `--channels server:marshal`
//! or `--dangerously-load-development-channels server:marshal`?
//!
//! Both the MCP server (writing the loud `instructions` warning) and
//! the `statusline` subcommand (writing the operator-visible suffix in
//! the status bar) call into this module.
//!
//! The MCP server's parent IS claude.exe directly — claude spawns
//! stdio MCP children via `child_process.spawn`. But the statusLine
//! command goes through a shell: Claude Code v2.1.x runs
//! `sh -c "/usr/local/bin/marshal-shim statusline"` (or `cmd.exe /c …`
//! on Windows) to honor any args in the configured command string, so
//! the shim's immediate parent there is `sh`/`cmd`, NOT claude. We
//! walk the ancestor chain up to a small bounded depth and check each
//! parent's cmdline; whichever ancestor is claude (cmdline contains
//! `--channels` or `--dangerously-load-development-channels`) carries
//! the answer.

/// Server name we expect to find in Claude's `--channels` / `--dev-channels`
/// argument value to mean "the marshal MCP server has the channel grant."
const MARSHAL_SERVER_TOKEN: &str = "server:marshal";

/// How far up the process ancestry to scan looking for a claude.exe
/// invocation carrying the channel-grant flag. Real chains are short:
/// MCP path is shim ← claude (depth 1); statusline path is shim ← sh
/// ← claude (depth 2). Bound at 6 so an unexpected supervisor /
/// terminal-multiplexer wrapper still resolves, and we don't walk
/// pathologically.
const ANCESTOR_WALK_LIMIT: usize = 6;

/// Resolve whether the marshal MCP server has the channel-push grant
/// by walking the shim's process ancestry to find the CLOSEST claude
/// process and reading its argv. Walks past shells and wrappers; stops
/// at the first ancestor whose argv[0] looks like a claude invocation,
/// because that's the binary whose grant decision actually governs
/// this MCP session. Returns `false` if we walk off the end without
/// finding a claude — the safe degradation is "assume off, surface the
/// warning"; tool calls and inbox-on-next-prompt still work either way.
pub fn marshal_channel_granted() -> bool {
    let Some(start) = current_ppid() else {
        return false;
    };
    let mut pid = start;
    for _ in 0..ANCESTOR_WALK_LIMIT {
        let argv = process_argv(pid);
        if !argv.is_empty() && argv_is_claude(&argv) {
            return argv_lists_marshal_channel(&argv);
        }
        let Some(parent) = process_parent_pid(pid) else {
            return false;
        };
        if parent <= 1 || parent == pid {
            return false;
        }
        pid = parent;
    }
    false
}

/// True when argv[0]'s basename (sans extension on Windows) is
/// `claude`. We deliberately don't match on substring elsewhere in
/// argv to avoid a shell wrapper that mentions the claude binary in
/// quoted args getting confused for claude itself.
fn argv_is_claude(argv: &[String]) -> bool {
    let Some(arg0) = argv.first() else {
        return false;
    };
    // Cross-platform basename: split on both `/` and `\` so Windows
    // paths in argv[0] work on Linux test runs too.
    let basename = arg0
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(arg0);
    // Drop a trailing extension (`.exe`, `.cmd`).
    let stem = basename
        .rsplit_once('.')
        .map(|(s, _)| s)
        .unwrap_or(basename);
    stem.eq_ignore_ascii_case("claude")
}

fn argv_lists_marshal_channel(argv: &[String]) -> bool {
    let mut iter = argv.iter().peekable();
    while let Some(arg) = iter.next() {
        let value: Option<&str> =
            if arg == "--channels" || arg == "--dangerously-load-development-channels" {
                iter.peek().map(|s| s.as_str())
            } else if let Some(v) = arg.strip_prefix("--channels=") {
                Some(v)
            } else {
                arg.strip_prefix("--dangerously-load-development-channels=")
            };
        if let Some(value) = value
            && value
                .split(',')
                .any(|tok| tok.trim() == MARSHAL_SERVER_TOKEN)
        {
            return true;
        }
    }
    false
}

#[cfg(unix)]
fn current_ppid() -> Option<u32> {
    Some(unsafe { libc::getppid() } as u32)
}

#[cfg(target_os = "linux")]
fn process_argv(pid: u32) -> Vec<String> {
    let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return Vec::new();
    };
    raw.split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

#[cfg(target_os = "linux")]
fn process_parent_pid(pid: u32) -> Option<u32> {
    let body = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("PPid:") {
            return rest.trim().parse().ok();
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn process_argv(pid: u32) -> Vec<String> {
    let Ok(out) = std::process::Command::new("ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    let s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.trim().split_whitespace().map(str::to_string).collect()
}

#[cfg(target_os = "macos")]
fn process_parent_pid(pid: u32) -> Option<u32> {
    let out = std::process::Command::new("ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()?.trim().parse().ok()
}

#[cfg(windows)]
fn current_ppid() -> Option<u32> {
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    let me = unsafe { GetCurrentProcessId() };
    process_parent_pid(me)
}

#[cfg(windows)]
fn process_parent_pid(pid: u32) -> Option<u32> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32, Process32First, Process32Next,
        TH32CS_SNAPPROCESS,
    };
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut entry: PROCESSENTRY32 = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32>() as u32;
        let mut found = None;
        if Process32First(snap, &mut entry) != 0 {
            loop {
                if entry.th32ProcessID == pid {
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

#[cfg(windows)]
fn process_argv(pid: u32) -> Vec<String> {
    use windows_sys::Wdk::System::Threading::{NtQueryInformationProcess, ProcessBasicInformation};
    use windows_sys::Win32::Foundation::{CloseHandle, UNICODE_STRING};
    use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PEB, PROCESS_BASIC_INFORMATION, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ,
        RTL_USER_PROCESS_PARAMETERS,
    };

    unsafe {
        let handle = OpenProcess(
            PROCESS_QUERY_INFORMATION | PROCESS_VM_READ,
            0,
            pid,
        );
        if handle.is_null() {
            return Vec::new();
        }

        // 1. ProcessBasicInformation → PebBaseAddress
        let mut pbi: PROCESS_BASIC_INFORMATION = std::mem::zeroed();
        let mut returned = 0u32;
        let status = NtQueryInformationProcess(
            handle,
            ProcessBasicInformation,
            &mut pbi as *mut _ as *mut _,
            std::mem::size_of::<PROCESS_BASIC_INFORMATION>() as u32,
            &mut returned,
        );
        if status < 0 {
            CloseHandle(handle);
            return Vec::new();
        }

        // 2. Read PEB at PebBaseAddress.
        let mut peb: PEB = std::mem::zeroed();
        let mut bytes_read = 0usize;
        if ReadProcessMemory(
            handle,
            pbi.PebBaseAddress as *const _,
            &mut peb as *mut _ as *mut _,
            std::mem::size_of::<PEB>(),
            &mut bytes_read,
        ) == 0
        {
            CloseHandle(handle);
            return Vec::new();
        }

        // 3. Read RTL_USER_PROCESS_PARAMETERS at PEB.ProcessParameters.
        let mut params: RTL_USER_PROCESS_PARAMETERS = std::mem::zeroed();
        if ReadProcessMemory(
            handle,
            peb.ProcessParameters as *const _,
            &mut params as *mut _ as *mut _,
            std::mem::size_of::<RTL_USER_PROCESS_PARAMETERS>(),
            &mut bytes_read,
        ) == 0
        {
            CloseHandle(handle);
            return Vec::new();
        }

        // 4. Read the CommandLine wide string.
        let cl: UNICODE_STRING = params.CommandLine;
        let len = cl.Length as usize;
        if len == 0 || cl.Buffer.is_null() {
            CloseHandle(handle);
            return Vec::new();
        }
        let mut buf: Vec<u16> = vec![0; len / 2];
        if ReadProcessMemory(
            handle,
            cl.Buffer as *const _,
            buf.as_mut_ptr() as *mut _,
            len,
            &mut bytes_read,
        ) == 0
        {
            CloseHandle(handle);
            return Vec::new();
        }
        CloseHandle(handle);

        let s = String::from_utf16_lossy(&buf);
        // Win32 CommandLine is a single string; split into argv by
        // CommandLineToArgvW semantics. For our needs (looking for
        // `--channels server:marshal` etc.) plain whitespace split is
        // accurate enough — the flag values we care about never embed
        // spaces or quotes.
        s.trim()
            .split_whitespace()
            .map(str::to_string)
            .collect()
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn current_ppid() -> Option<u32> {
    None
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn process_argv(_pid: u32) -> Vec<String> {
    Vec::new()
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn process_parent_pid(_pid: u32) -> Option<u32> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_server_marshal_in_dev_channels() {
        let argv = vec![
            "claude".to_string(),
            "--dangerously-load-development-channels".to_string(),
            "server:marshal".to_string(),
        ];
        assert!(argv_lists_marshal_channel(&argv));
    }

    #[test]
    fn detects_server_marshal_in_approved_channels() {
        let argv = vec![
            "claude".to_string(),
            "--channels".to_string(),
            "server:marshal".to_string(),
        ];
        assert!(argv_lists_marshal_channel(&argv));
    }

    #[test]
    fn detects_marshal_in_comma_separated_list() {
        let argv = vec![
            "claude".to_string(),
            "--channels".to_string(),
            "server:other,server:marshal,plugin:foo".to_string(),
        ];
        assert!(argv_lists_marshal_channel(&argv));
    }

    #[test]
    fn detects_equals_form() {
        let argv = vec![
            "claude".to_string(),
            "--dangerously-load-development-channels=server:marshal".to_string(),
        ];
        assert!(argv_lists_marshal_channel(&argv));
    }

    #[test]
    fn missing_flag_returns_false() {
        let argv = vec!["claude".to_string()];
        assert!(!argv_lists_marshal_channel(&argv));
    }

    #[test]
    fn other_server_in_channels_doesnt_grant_marshal() {
        let argv = vec![
            "claude".to_string(),
            "--channels".to_string(),
            "server:rship,plugin:foo".to_string(),
        ];
        assert!(!argv_lists_marshal_channel(&argv));
    }

    #[test]
    fn empty_argv_returns_false() {
        assert!(!argv_lists_marshal_channel(&[]));
    }

    #[test]
    fn argv_is_claude_matches_basename() {
        assert!(argv_is_claude(&["claude".into()]));
        assert!(argv_is_claude(&["/root/.local/bin/claude".into()]));
        assert!(argv_is_claude(&["C:\\path\\to\\claude.exe".into()]));
        assert!(argv_is_claude(&["Claude".into()])); // case-insensitive
        assert!(!argv_is_claude(&["sh".into()]));
        assert!(!argv_is_claude(&["bash".into()]));
        assert!(!argv_is_claude(&["claude-code-other".into()]));
        assert!(!argv_is_claude(&[])); // empty
    }
}
