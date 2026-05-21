# Claude Code statusLine helper for the marshal-shim plugin (Windows).
#
# Renders a PS1-style prefix (`[user@host dir]`) followed by the
# marshal nickname for this Claude Code session, e.g.
#
#     [trevor@laptop marshal] marshal-3
#
# Mechanism: marshal-shim writes
#   %LOCALAPPDATA%\marshal\state\shim-by-ppid-<ppid>.json
# on startup and on every liveness tick. The shim and this script
# are both subprocesses of the same Claude Code process, so they
# share a PPID — we look up the file keyed by our own PPID.

$ErrorActionPreference = 'SilentlyContinue'

# --- Read Claude Code's JSON payload from stdin ----------------------------

$payload = [Console]::In.ReadToEnd()
$cwd = $null
if ($payload) {
    try {
        $obj = $payload | ConvertFrom-Json
        if ($obj.workspace -and $obj.workspace.current_dir) {
            $cwd = $obj.workspace.current_dir
        }
        elseif ($obj.cwd) {
            $cwd = $obj.cwd
        }
    } catch { }
}
if (-not $cwd) { $cwd = (Get-Location).Path }

# --- PS1-style prefix -------------------------------------------------------

$userName = $env:USERNAME
$hostName = $env:COMPUTERNAME
$dir      = Split-Path -Leaf $cwd
$prefix   = "[$userName@$hostName $dir]"

# --- Marshal nickname (from the shim's PPID-keyed state file) --------------

$ppid = $null
try {
    $ppid = (Get-CimInstance Win32_Process -Filter "ProcessId=$PID").ParentProcessId
} catch { }

$nickname = $null
if ($ppid) {
    $stateDir  = Join-Path $env:LOCALAPPDATA 'marshal\state'
    $stateFile = Join-Path $stateDir "shim-by-ppid-$ppid.json"
    if (Test-Path $stateFile) {
        try {
            $st = Get-Content $stateFile -Raw | ConvertFrom-Json
            $nickname = $st.nickname
        } catch { }
    }
}

if ($nickname) {
    Write-Output "$prefix $nickname"
} else {
    Write-Output $prefix
}
