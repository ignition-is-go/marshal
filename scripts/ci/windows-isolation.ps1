[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$RunnerRoot,
    [Parameter(Mandatory)][ValidateRange(1, 65536)][int]$MaxCpu,
    [Parameter(Mandatory)][ValidateRange(1, [long]::MaxValue)][long]$MaxMemoryBytes,
    [Parameter(Mandatory)][ValidateRange(1, 1024)][int]$ExpectedDiskCount,
    [string[]]$ForbiddenPath = @('C:\host', '\\.\pipe\docker_engine', '\\.\pipe\containerd-containerd', '\\.\PhysicalDrive1'),
    [string[]]$ForbiddenEnvironment = @('AWS_ACCESS_KEY_ID', 'AWS_SECRET_ACCESS_KEY', 'AZURE_CLIENT_SECRET', 'KUBECONFIG', 'TF_TOKEN_app_terraform_io', 'PVE_API_TOKEN'),
    [string[]]$BlockedEndpoint = @(),
    [switch]$RunNetworkChecks
)

$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'windows_isolation.psm1') -Force -DisableNameChecking
if ($env:OS -ne 'Windows_NT') { throw 'Run the pool recipe inside its Windows guest.' }
if (-not [IO.Path]::IsPathRooted($RunnerRoot)) { throw 'RunnerRoot must be an absolute guest path.' }
if (-not (Test-Path -LiteralPath (Join-Path $RunnerRoot 'run.cmd'))) { throw 'Runner installation is absent.' }
if (-not $RunNetworkChecks -and $BlockedEndpoint.Count -gt 0) { throw 'Endpoint checks require -RunNetworkChecks.' }
if ($RunNetworkChecks -and $BlockedEndpoint.Count -eq 0) { throw 'Supply explicit endpoints for network checks.' }

$computer = Get-CimInstance Win32_ComputerSystem
$disks = @(Get-CimInstance Win32_DiskDrive)
Assert-ResourceLimits -Cpu $computer.NumberOfLogicalProcessors -Memory $computer.TotalPhysicalMemory -Disks $disks.Count -MaxCpu $MaxCpu -MaxMemory $MaxMemoryBytes -ExpectedDisks $ExpectedDiskCount
Assert-PathsAbsent -Paths $ForbiddenPath
Assert-CredentialsAbsent -Names $ForbiddenEnvironment
Assert-FreshMarker -Path (Join-Path $RunnerRoot '_work/.marshal-isolation-marker')
foreach ($endpoint in $BlockedEndpoint) {
    $uri = [Uri]"tcp://$endpoint"
    if ($uri.Port -lt 1 -or $uri.AbsolutePath -ne '/' -or $uri.UserInfo -or $uri.Query -or $uri.Fragment) { throw "Expected host:port endpoint: $endpoint" }
    Assert-TcpBlocked -HostName $uri.Host -Port $uri.Port
}
Write-Output 'Supplied resource, marker, path, environment, and optional endpoint checks passed.'
