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
//! the `statusline` subcommand (writing the `⚠ NO LIVE PUSH` suffix in
//! the operator-visible status bar) call into this module. They're
//! sibling subprocesses spawned by the same claude.exe, so both
//! getppid() to the same PID; only the parent's cmdline is read.

/// Server name we expect to find in Claude's `--channels` / `--dev-channels`
/// argument value to mean "the marshal MCP server has the channel grant."
const MARSHAL_SERVER_TOKEN: &str = "server:marshal";

/// Resolve whether marshal-server has channel-push grant from the
/// shim's parent process (= claude.exe). Returns `false` on platforms
/// or sandboxes where we can't read the parent's argv — the safe
/// degradation for the warning surface is "assume off, surface the
/// warning"; tool calls and inbox-on-next-prompt still work either
/// way.
pub fn marshal_channel_granted() -> bool {
    let argv = parent_argv();
    if argv.is_empty() {
        return false;
    }
    argv_lists_marshal_channel(&argv)
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

#[cfg(target_os = "linux")]
fn parent_argv() -> Vec<String> {
    let ppid = unsafe { libc::getppid() } as u32;
    let Ok(raw) = std::fs::read(format!("/proc/{ppid}/cmdline")) else {
        return Vec::new();
    };
    raw.split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

#[cfg(target_os = "macos")]
fn parent_argv() -> Vec<String> {
    let ppid = unsafe { libc::getppid() } as u32;
    let Ok(out) = std::process::Command::new("ps")
        .args(["-o", "command=", "-p", &ppid.to_string()])
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

#[cfg(windows)]
fn parent_argv() -> Vec<String> {
    use windows_sys::Wdk::System::Threading::{NtQueryInformationProcess, ProcessBasicInformation};
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE, UNICODE_STRING};
    use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32, Process32First, Process32Next,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcessId, OpenProcess, PEB, PROCESS_BASIC_INFORMATION,
        PROCESS_QUERY_INFORMATION, PROCESS_VM_READ, RTL_USER_PROCESS_PARAMETERS,
    };

    let Some(ppid) = (unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            None
        } else {
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
    }) else {
        return Vec::new();
    };

    unsafe {
        let handle = OpenProcess(
            PROCESS_QUERY_INFORMATION | PROCESS_VM_READ,
            0,
            ppid,
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
fn parent_argv() -> Vec<String> {
    Vec::new()
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
}
