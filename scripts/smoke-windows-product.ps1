param(
    [Parameter(Mandatory = $true, Position = 0)]
    [string]$FeanorFSBin,

    [Parameter(Mandatory = $true, Position = 1)]
    [string]$FeanorFSTrayBin,

    [switch]$RequireAuthenticode
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$sourceCli = (Resolve-Path -LiteralPath $FeanorFSBin).Path
$sourceTray = (Resolve-Path -LiteralPath $FeanorFSTrayBin).Path
if ($RequireAuthenticode) {
    foreach ($binary in @($sourceCli, $sourceTray)) {
        $signature = Get-AuthenticodeSignature -FilePath $binary
        if ($signature.Status -ne [System.Management.Automation.SignatureStatus]::Valid) {
            throw "$(Split-Path -Leaf $binary) does not have a valid Authenticode signature."
        }
    }
}

$root = Join-Path ([IO.Path]::GetTempPath()) ("feanorfs-windows-product-" + [Guid]::NewGuid())
$profileHome = Join-Path $root "home"
$binDir = Join-Path $root "bin"
$workspace = Join-Path $root "workspace"
$startLog = Join-Path $root "start.log"
$cli = Join-Path $binDir "feanorfs.exe"
$tray = Join-Path $binDir "feanorfs-tray.exe"
$taskPath = "\FeanorFS\"
$credentialTargets = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)

function Get-SmokeTasks {
    return @(Get-ScheduledTask -TaskPath $taskPath -ErrorAction SilentlyContinue)
}

function Remove-SmokeTasks {
    foreach ($task in @(Get-SmokeTasks)) {
        Stop-ScheduledTask -TaskPath $task.TaskPath -TaskName $task.TaskName -ErrorAction SilentlyContinue
        Unregister-ScheduledTask -TaskPath $task.TaskPath -TaskName $task.TaskName -Confirm:$false -ErrorAction SilentlyContinue
    }
}

function Stop-SmokeProcesses {
    foreach ($process in @(Get-Process -Name "feanorfs", "feanorfs-tray" -ErrorAction SilentlyContinue)) {
        try {
            if ($process.Path -and $process.Path.StartsWith($binDir, [StringComparison]::OrdinalIgnoreCase)) {
                Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue
            }
        }
        catch {
            # The process may exit between enumeration and inspection.
        }
    }
}

function Wait-For([scriptblock]$Condition, [string]$Description, [int]$Seconds = 15) {
    $deadline = [DateTime]::UtcNow.AddSeconds($Seconds)
    while ([DateTime]::UtcNow -lt $deadline) {
        try {
            if (& $Condition) {
                return
            }
        }
        catch {
            # The asynchronous service may not be queryable yet.
        }
        Start-Sleep -Milliseconds 250
    }
    throw "Timed out waiting for $Description."
}

function Invoke-Start {
    Push-Location $workspace
    try {
        & $cli start $workspace *> $startLog
        if ($LASTEXITCODE -ne 0) {
            throw "feanorfs start failed with exit code $LASTEXITCODE."
        }
    }
    finally {
        Pop-Location
    }
}

function Normalize-WindowsPath([string]$Path) {
    $value = $Path.Trim('"')
    if ($value.StartsWith("\\?\", [StringComparison]::Ordinal)) {
        $value = $value.Substring(4)
    }
    return [IO.Path]::GetFullPath($value).TrimEnd('\')
}

function Remember-CredentialTarget([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        return $null
    }
    $config = Get-Content -LiteralPath $Path -Raw | ConvertFrom-Json
    $storeProperty = $config.PSObject.Properties["credential_store"]
    $idProperty = $config.PSObject.Properties["credential_id"]
    if ($storeProperty -and $idProperty -and $storeProperty.Value -eq "os" -and [string]$idProperty.Value -match '^fsc1-[0-9a-f]{32}$') {
        [void]$script:credentialTargets.Add(([string]$idProperty.Value) + ".com.feanorfs.credentials")
    }
    return $config
}

function Assert-RedactedCredentialConfig([string]$Path) {
    $config = Remember-CredentialTarget $Path
    $storeProperty = if ($config) { $config.PSObject.Properties["credential_store"] } else { $null }
    $idProperty = if ($config) { $config.PSObject.Properties["credential_id"] } else { $null }
    if (-not $storeProperty -or -not $idProperty -or $storeProperty.Value -ne "os" -or [string]$idProperty.Value -notmatch '^fsc1-[0-9a-f]{32}$') {
        throw "FeanorFS did not persist a valid Windows Credential Manager reference."
    }
    $properties = @($config.PSObject.Properties.Name)
    if ($properties -contains "encryption_password" -or $properties -contains "server_password") {
        throw "FeanorFS left an encryption key or server token in Windows config JSON."
    }
}

function Assert-TaskAction([object]$Task, [string]$ExpectedProgram, [string]$ArgumentPattern) {
    $actions = @($Task.Actions)
    if ($actions.Count -ne 1) {
        throw "Task $($Task.TaskName) has an unexpected action count."
    }
    $action = $actions[0]
    if ((Normalize-WindowsPath ([string]$action.Execute)) -ine (Normalize-WindowsPath $ExpectedProgram)) {
        throw "Task $($Task.TaskName) runs an unexpected executable."
    }
    $arguments = [string]$action.Arguments
    if ($arguments -notmatch $ArgumentPattern) {
        throw "Task $($Task.TaskName) has an unexpected argument shape."
    }
    if ($arguments -match '(?i)--token|--password|fnh1-|fnr1-|fnp1-|fnp2-') {
        throw "Task $($Task.TaskName) exposes a credential or capability in its arguments."
    }
}

function Assert-HealthyProduct {
    Wait-For {
        $tasks = @(Get-SmokeTasks)
        $tasks.Count -eq 3 -and @($tasks | Where-Object State -ne "Running").Count -eq 0
    } "hub, workspace, and tray tasks to run"

    $tasks = @(Get-SmokeTasks)
    $hubTask = $tasks | Where-Object TaskName -eq "com.feanorfs.hub"
    $trayTask = $tasks | Where-Object TaskName -eq "Tray"
    $workspaceTasks = @($tasks | Where-Object TaskName -notin @("com.feanorfs.hub", "Tray"))
    if (-not $hubTask -or -not $trayTask -or $workspaceTasks.Count -ne 1) {
        throw "Windows product did not install exactly one hub, workspace, and tray task."
    }

    $listenPortPath = Join-Path $profileHome ".feanorfs\hub-data\listen-port"
    $listenPort = 0
    if (-not (Test-Path -LiteralPath $listenPortPath -PathType Leaf) -or
        -not [int]::TryParse((Get-Content -LiteralPath $listenPortPath -Raw).Trim(), [ref]$listenPort) -or
        $listenPort -lt 1 -or $listenPort -gt 65535) {
        throw "Windows automatic private hub did not persist a valid listen port."
    }

    Assert-TaskAction $hubTask $cli '^service hub-run ".+"$'
    Assert-TaskAction $workspaceTasks[0] $cli '^service run ".+"$'
    Assert-TaskAction $trayTask $tray '^$'
    if ([string]$trayTask.Principal.LogonType -ne "InteractiveToken") {
        throw "Windows tray task is not registered in the interactive user session."
    }

    Assert-RedactedCredentialConfig (Join-Path $profileHome ".feanorfs\global.json")
    Assert-RedactedCredentialConfig (Join-Path $workspace ".feanorfs\config.json")

    Push-Location $workspace
    try {
        $doctorOutput = & $cli --json doctor
        if ($LASTEXITCODE -ne 0) {
            throw "feanorfs doctor reported an unhealthy Windows product."
        }
        $doctor = $doctorOutput | ConvertFrom-Json
        $expectedChecks = @(
            "automatic_sync",
            "e2ee",
            "global_config",
            "local_state",
            "private_hub",
            "remote_workspace",
            "server",
            "tray_registration",
            "workspace_config",
            "workspace_format"
        ) | Sort-Object
        $actualChecks = @($doctor.checks.name) | Sort-Object
        if (-not $doctor.ok -or (Compare-Object $expectedChecks $actualChecks) -or @($doctor.checks | Where-Object status -ne "ok").Count -ne 0) {
            throw "Windows doctor checks did not all pass."
        }

        $trayStatus = (& $cli --json tray status) | ConvertFrom-Json
        if ($LASTEXITCODE -ne 0 -or $trayStatus.mirror_state -ne "idle" -or -not $trayStatus.watching -or $trayStatus.paused) {
            throw "Windows tray status is not idle and watched."
        }

        $mcpInput = @(
            '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}',
            '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"sync_status","arguments":{}}}'
        )
        $mcpResponses = @($mcpInput | & $cli mcp | ForEach-Object { $_ | ConvertFrom-Json })
        if ($LASTEXITCODE -ne 0 -or $mcpResponses.Count -ne 2) {
            throw "Windows MCP smoke returned an unexpected response count."
        }
        $syncStatus = $mcpResponses | Where-Object id -eq 2
        if ($syncStatus.result.mirror_state -ne "idle" -or $syncStatus.result.local_file_count -ne 1 -or @($syncStatus.result.upload_required).Count -ne 0 -or @($syncStatus.result.download_required).Count -ne 0) {
            throw "Windows MCP sync status is not idle."
        }
    }
    finally {
        Pop-Location
    }
}

try {
    if (@(Get-SmokeTasks).Count -ne 0) {
        throw "Windows product smoke refuses to replace existing FeanorFS scheduled tasks."
    }
    New-Item -ItemType Directory -Path $profileHome, $binDir, $workspace | Out-Null
    Copy-Item -LiteralPath $sourceCli -Destination $cli
    Copy-Item -LiteralPath $sourceTray -Destination $tray
    [IO.File]::WriteAllText((Join-Path $workspace "windows-smoke.txt"), "Windows product smoke`n")

    $env:HOME = $profileHome
    $env:USERPROFILE = $profileHome
    $env:FEANORFS_TRAY_BIN = $tray
    # Require the real native store; cleanup removes the exact random entries.
    $env:FEANORFS_CREDENTIAL_STORE = "os"

    $version = & $cli --version
    if ($LASTEXITCODE -ne 0 -or $version -notmatch '^feanorfs [0-9]') {
        throw "Windows CLI did not report its version."
    }

    Invoke-Start
    Assert-HealthyProduct

    & $cli stop $workspace *> (Join-Path $root "stop.log")
    if ($LASTEXITCODE -ne 0) {
        throw "feanorfs stop failed on Windows."
    }
    if (-not (Test-Path -LiteralPath (Join-Path $workspace "windows-smoke.txt")) -or -not (Test-Path -LiteralPath (Join-Path $workspace ".feanorfs\config.json"))) {
        throw "Windows stop did not preserve the working file and encrypted setup."
    }
    Wait-For {
        @(Get-SmokeTasks | Where-Object TaskName -notin @("com.feanorfs.hub", "Tray")).Count -eq 0
    } "workspace task removal"

    Invoke-Start
    Assert-HealthyProduct
    Write-Host "Windows product smoke passed: one-command host, Credential Manager, Task Scheduler services, interactive tray, TLS, doctor, MCP, and reversible stop/resume."
}
finally {
    $credentialCleanupFailed = $false
    foreach ($configPath in @(
        (Join-Path $profileHome ".feanorfs\global.json"),
        (Join-Path $workspace ".feanorfs\config.json")
    )) {
        try {
            $null = Remember-CredentialTarget $configPath
        }
        catch {
            $credentialCleanupFailed = $true
        }
    }
    if (Test-Path -LiteralPath $cli) {
        & $cli stop $workspace *> $null
    }
    Remove-SmokeTasks
    Stop-SmokeProcesses
    foreach ($target in $credentialTargets) {
        & "$env:SystemRoot\System32\cmdkey.exe" "/delete:$target" *> $null
        if ($LASTEXITCODE -ne 0) {
            $credentialCleanupFailed = $true
        }
    }
    for ($attempt = 0; $attempt -lt 20 -and (Test-Path -LiteralPath $root); $attempt++) {
        try {
            Remove-Item -LiteralPath $root -Recurse -Force
        }
        catch {
            Start-Sleep -Milliseconds 250
        }
    }
    if ($credentialCleanupFailed) {
        throw "Windows product smoke could not remove its temporary Credential Manager entries."
    }
}
