$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'windows_isolation.psm1') -Force -DisableNameChecking

function Assert-Throws {
    param([scriptblock]$Operation)
    $thrown = $false
    try { & $Operation } catch { $thrown = $true }
    if (-not $thrown) { throw 'Expected operation to fail.' }
}

Assert-ResourceLimits -Cpu 4 -Memory 100 -Disks 1 -MaxCpu 8 -MaxMemory 200 -ExpectedDisks 1
Assert-Throws { Assert-ResourceLimits -Cpu 9 -Memory 100 -Disks 1 -MaxCpu 8 -MaxMemory 200 -ExpectedDisks 1 }
Assert-Throws { Assert-ResourceLimits -Cpu 4 -Memory 201 -Disks 1 -MaxCpu 8 -MaxMemory 200 -ExpectedDisks 1 }
Assert-Throws { Assert-ResourceLimits -Cpu 4 -Memory 100 -Disks 2 -MaxCpu 8 -MaxMemory 200 -ExpectedDisks 1 }
Assert-Throws { Assert-ResourceLimits -Cpu 4 -Memory 100 -Disks 1 -MaxCpu 0 -MaxMemory 200 -ExpectedDisks 1 }

if ((Resolve-TcpDisposition -State Connected) -ne 'reachable') { throw 'Successful connections must fail isolation.' }
if ((Resolve-TcpDisposition -State TimedOut) -ne 'blocked') { throw 'Timeouts must count as denied.' }
$refused = [Net.Sockets.SocketException]::new([int][Net.Sockets.SocketError]::ConnectionRefused)
$aggregate = [AggregateException]::new($refused)
if ((Resolve-TcpDisposition -State Failed -Failure $aggregate) -ne 'blocked') { throw 'Connection refusal must count as denied.' }
Assert-Throws { Resolve-TcpDisposition -State Failed -Failure ([InvalidOperationException]::new('Unexpected fixture')) }

$directory = Join-Path ([IO.Path]::GetTempPath()) ([guid]::NewGuid().ToString())
New-Item -ItemType Directory -Path $directory | Out-Null
try {
    $marker = Join-Path $directory 'marker'
    Assert-FreshMarker -Path $marker
    Assert-Throws { Assert-FreshMarker -Path $marker }
    Assert-PathsAbsent -Paths @((Join-Path $directory 'missing'))
    Assert-Throws { Assert-PathsAbsent -Paths @($marker) }
    $name = 'MARSHAL_ISOLATION_TEST_' + [guid]::NewGuid().ToString('N')
    Assert-CredentialsAbsent -Names @($name)
    try {
        [Environment]::SetEnvironmentVariable($name, 'fixture-value')
        Assert-Throws { Assert-CredentialsAbsent -Names @($name) }
    } finally {
        [Environment]::SetEnvironmentVariable($name, $null)
    }

    $listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
    $listener.Start()
    $port = $listener.LocalEndpoint.Port
    try {
        Assert-Throws { Assert-TcpBlocked -HostName '127.0.0.1' -Port $port }
    } finally {
        $listener.Stop()
    }
    Assert-TcpBlocked -HostName '127.0.0.1' -Port $port
} finally {
    Remove-Item -LiteralPath $directory -Recurse -Force
}

$tokens = $null
$parseErrors = $null
[Management.Automation.Language.Parser]::ParseFile((Join-Path $PSScriptRoot 'windows-isolation.ps1'), [ref]$tokens, [ref]$parseErrors) | Out-Null
if ($parseErrors.Count -gt 0) { throw ($parseErrors | Out-String) }
Write-Output 'Windows isolation offline checks passed; no external endpoints were contacted.'
