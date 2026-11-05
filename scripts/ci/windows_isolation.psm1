Set-StrictMode -Version Latest

function Assert-ResourceLimits {
    param([int]$Cpu, [long]$Memory, [int]$Disks, [int]$MaxCpu, [long]$MaxMemory, [int]$ExpectedDisks)
    if ($MaxCpu -le 0 -or $MaxMemory -le 0 -or $ExpectedDisks -le 0) {
        throw 'Expected CPU, memory, and disk limits must be positive.'
    }
    if ($Cpu -le 0 -or $Memory -le 0 -or $Cpu -gt $MaxCpu -or $Memory -gt $MaxMemory -or $Disks -ne $ExpectedDisks) {
        throw 'Observed resources do not match the supplied pool limits.'
    }
}

function Assert-FreshMarker {
    param([Parameter(Mandatory)][string]$Path)
    if (Test-Path -LiteralPath $Path) { throw 'State persisted at the supplied marker path.' }
    Set-Content -LiteralPath $Path -Value ([guid]::NewGuid().ToString()) -ErrorAction Stop
}

function Assert-PathsAbsent {
    param([string[]]$Paths)
    foreach ($path in $Paths) {
        if (Test-Path -LiteralPath $path -ErrorAction Stop) { throw "Forbidden resource is visible: $path" }
    }
}

function Assert-CredentialsAbsent {
    param([string[]]$Names)
    foreach ($name in $Names) {
        if ($null -ne [Environment]::GetEnvironmentVariable($name)) {
            throw "Forbidden environment variable is present: $name"
        }
    }
}

function Resolve-TcpDisposition {
    param(
        [ValidateSet('Connected', 'TimedOut', 'Failed')][string]$State,
        [System.Exception]$Failure
    )
    if ($State -eq 'Connected') { return 'reachable' }
    if ($State -eq 'TimedOut') { return 'blocked' }
    if ($null -eq $Failure) { throw 'A failed connection requires an error.' }
    $cause = $Failure.GetBaseException()
    if ($cause -is [Net.Sockets.SocketException] -and $cause.SocketErrorCode -in @(
        [Net.Sockets.SocketError]::ConnectionRefused,
        [Net.Sockets.SocketError]::TimedOut,
        [Net.Sockets.SocketError]::NetworkUnreachable,
        [Net.Sockets.SocketError]::HostUnreachable
    )) { return 'blocked' }
    throw $cause
}

function Assert-TcpBlocked {
    param([string]$HostName, [ValidateRange(1, 65535)][int]$Port, [ValidateRange(1, 60000)][int]$TimeoutMilliseconds = 3000)
    $client = [Net.Sockets.TcpClient]::new()
    try {
        $task = $client.ConnectAsync($HostName, $Port)
        try {
            $finished = $task.Wait($TimeoutMilliseconds)
            $state = if ($finished -and $client.Connected) { 'Connected' } else { 'TimedOut' }
            $disposition = Resolve-TcpDisposition -State $state
        } catch {
            $disposition = Resolve-TcpDisposition -State Failed -Failure $_.Exception
        }
        if ($disposition -ne 'blocked') { throw "Endpoint is reachable: ${HostName}:$Port" }
    } finally {
        $client.Dispose()
    }
}

Export-ModuleMember -Function Assert-ResourceLimits, Assert-FreshMarker, Assert-PathsAbsent, Assert-CredentialsAbsent, Resolve-TcpDisposition, Assert-TcpBlocked
