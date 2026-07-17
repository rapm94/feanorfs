$ErrorActionPreference = "Stop"

$root = Join-Path ([IO.Path]::GetTempPath()) ("feanorfs-install-routing-" + [Guid]::NewGuid())
$installDir = Join-Path $root "bin"
$marker = Join-Path $root "cli-installed"
$originalArchitecture = $env:PROCESSOR_ARCHITECTURE
New-Item -ItemType Directory -Path $root | Out-Null

$global:FeanorFSInstallerTestRelease = $null
$global:FeanorFSInstallerTestDownloads = @{}
$global:FeanorFSInstallerTestLaunches = @()

function global:Invoke-RestMethod {
    param(
        [switch]$UseBasicParsing,
        [string]$Uri
    )
    return $global:FeanorFSInstallerTestRelease
}

function global:Invoke-WebRequest {
    param(
        [switch]$UseBasicParsing,
        [string]$Uri,
        [string]$OutFile
    )
    if (-not $global:FeanorFSInstallerTestDownloads.ContainsKey($Uri)) {
        throw "Unexpected installer test URL: $Uri"
    }
    [IO.File]::WriteAllBytes($OutFile, $global:FeanorFSInstallerTestDownloads[$Uri])
}

function global:Get-AuthenticodeSignature {
    param([string]$FilePath)
    return [pscustomobject]@{ Status = "Valid" }
}

function global:Get-Process {
    param(
        [string]$Name,
        [object]$ErrorAction
    )
    return @()
}

function global:Start-Process {
    param(
        [string]$FilePath,
        [string[]]$ArgumentList,
        [string]$WorkingDirectory
    )
    $global:FeanorFSInstallerTestLaunches += [pscustomobject]@{
        FilePath = $FilePath
        ArgumentList = @($ArgumentList)
    }
}

function New-Asset([string]$name) {
    return [pscustomobject]@{
        name = $name
        browser_download_url = "https://example.invalid/$name"
    }
}

try {
    $env:FEANORFS_RELEASE_API = "https://example.invalid/releases/latest"
    $env:FEANORFS_INSTALL_DIR = $installDir
    $env:FEANORFS_TEST_CLI_MARKER = $marker

    $cliInstaller = @'
param()
[IO.File]::WriteAllText($env:FEANORFS_TEST_CLI_MARKER, "installed")
'@
    $global:FeanorFSInstallerTestDownloads["https://example.invalid/feanorfs-client-installer.ps1"] =
        [Text.Encoding]::UTF8.GetBytes($cliInstaller)
    $global:FeanorFSInstallerTestRelease = [pscustomobject]@{
        tag_name = "v9.9.9"
        assets = @((New-Asset "feanorfs-client-installer.ps1"))
    }
    & "$PSScriptRoot/install.ps1" *> (Join-Path $root "fallback.log")
    if (-not (Test-Path -PathType Leaf $marker)) {
        throw "Legacy Windows release did not use the CLI fallback."
    }

    Remove-Item -Force $marker
    $global:FeanorFSInstallerTestRelease = [pscustomobject]@{
        tag_name = "v9.9.9"
        assets = @((New-Asset "FeanorFS-windows-x86_64.zip"))
    }
    $failedClosed = $false
    try {
        & "$PSScriptRoot/install.ps1" *> (Join-Path $root "missing-checksum.log")
    }
    catch {
        $failedClosed = $_.Exception.Message -like "*without its checksum*"
    }
    if (-not $failedClosed -or (Test-Path $marker)) {
        throw "Windows desktop bundle without a checksum did not fail closed."
    }

    $global:FeanorFSInstallerTestDownloads["https://example.invalid/FeanorFS-windows-x86_64.zip"] =
        [Text.Encoding]::UTF8.GetBytes("not-a-zip")
    $global:FeanorFSInstallerTestDownloads["https://example.invalid/FeanorFS-windows-x86_64.zip.sha256"] =
        [Text.Encoding]::ASCII.GetBytes(("0" * 64) + "  FeanorFS-windows-x86_64.zip`n")
    $global:FeanorFSInstallerTestRelease = [pscustomobject]@{
        tag_name = "v9.9.9"
        assets = @(
            (New-Asset "FeanorFS-windows-x86_64.zip"),
            (New-Asset "FeanorFS-windows-x86_64.zip.sha256")
        )
    }
    $failedClosed = $false
    try {
        & "$PSScriptRoot/install.ps1" *> (Join-Path $root "bad-checksum.log")
    }
    catch {
        $failedClosed = $_.Exception.Message -like "*checksum does not match*"
    }
    if (-not $failedClosed -or (Test-Path $marker)) {
        throw "Windows desktop bundle with a bad checksum did not fail closed."
    }

    $payload = Join-Path $root "desktop-payload"
    New-Item -ItemType Directory -Path $payload | Out-Null
    [IO.File]::WriteAllBytes((Join-Path $payload "feanorfs.exe"), [byte[]](1, 2, 3))
    [IO.File]::WriteAllBytes((Join-Path $payload "feanorfs-tray.exe"), [byte[]](4, 5, 6))
    $archiveFixture = Join-Path $root "FeanorFS-windows-x86_64.zip"
    Compress-Archive -Path (Join-Path $payload "*") -DestinationPath $archiveFixture
    $archiveHash = (Get-FileHash -Algorithm SHA256 $archiveFixture).Hash.ToLowerInvariant()
    $global:FeanorFSInstallerTestDownloads["https://example.invalid/FeanorFS-windows-x86_64.zip"] =
        [IO.File]::ReadAllBytes($archiveFixture)
    $global:FeanorFSInstallerTestDownloads["https://example.invalid/FeanorFS-windows-x86_64.zip.sha256"] =
        [Text.Encoding]::ASCII.GetBytes("$archiveHash  FeanorFS-windows-x86_64.zip`n")
    $global:FeanorFSInstallerTestRelease = [pscustomobject]@{
        tag_name = "v9.9.9"
        assets = @(
            (New-Asset "FeanorFS-windows-x86_64.zip"),
            (New-Asset "FeanorFS-windows-x86_64.zip.sha256")
        )
    }
    $env:PROCESSOR_ARCHITECTURE = "AMD64"
    $env:FEANORFS_INSTALLER_TEST_NO_SHORTCUT = "1"
    $env:FEANORFS_INSTALLER_TEST_NO_PATH_UPDATE = "1"
    $env:FEANORFS_INSTALLER_TEST_FORCE_TRAY_LAUNCH = "1"
    & "$PSScriptRoot/install.ps1" *> (Join-Path $root "desktop-success.log")
    $installedTray = Join-Path $installDir "feanorfs-tray.exe"
    if (
        -not (Test-Path -PathType Leaf (Join-Path $installDir "feanorfs.exe")) -or
        -not (Test-Path -PathType Leaf $installedTray) -or
        $global:FeanorFSInstallerTestLaunches.Count -ne 1 -or
        $global:FeanorFSInstallerTestLaunches[0].FilePath -ne $installedTray -or
        @($global:FeanorFSInstallerTestLaunches[0].ArgumentList).Count -ne 1 -or
        $global:FeanorFSInstallerTestLaunches[0].ArgumentList[0] -ne "--first-run"
    ) {
        throw "Verified Windows desktop installation did not launch the installed tray exactly once."
    }
    $successLog = Get-Content -Raw (Join-Path $root "desktop-success.log")
    if ($successLog -notlike "*FeanorFS is now in your system tray*" -or
        $successLog -notlike "*no terminal setup is required*") {
        throw "Windows desktop installer did not hand off to tray-first onboarding."
    }
    $global:FeanorFSInstallerTestLaunches = @()
    $env:FEANORFS_NO_LAUNCH = "1"
    & "$PSScriptRoot/install.ps1" *> (Join-Path $root "desktop-no-launch.log")
    if ($global:FeanorFSInstallerTestLaunches.Count -ne 0) {
        throw "Windows desktop installer ignored FEANORFS_NO_LAUNCH."
    }
    $noLaunchLog = Get-Content -Raw (Join-Path $root "desktop-no-launch.log")
    if ($noLaunchLog -notlike "*Headless setup: feanorfs start*") {
        throw "Windows no-launch installer path did not provide the headless setup command."
    }

    Write-Host "Windows installer routing passed: legacy fallback, fail-closed desktop verification, and tray-first launch work."
}
finally {
    Remove-Item function:global:Invoke-RestMethod -ErrorAction SilentlyContinue
    Remove-Item function:global:Invoke-WebRequest -ErrorAction SilentlyContinue
    Remove-Item function:global:Get-AuthenticodeSignature -ErrorAction SilentlyContinue
    Remove-Item function:global:Get-Process -ErrorAction SilentlyContinue
    Remove-Item function:global:Start-Process -ErrorAction SilentlyContinue
    Remove-Variable -Name FeanorFSInstallerTestRelease -Scope Global -ErrorAction SilentlyContinue
    Remove-Variable -Name FeanorFSInstallerTestDownloads -Scope Global -ErrorAction SilentlyContinue
    Remove-Variable -Name FeanorFSInstallerTestLaunches -Scope Global -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force $root -ErrorAction SilentlyContinue
    Remove-Item Env:FEANORFS_RELEASE_API -ErrorAction SilentlyContinue
    Remove-Item Env:FEANORFS_INSTALL_DIR -ErrorAction SilentlyContinue
    Remove-Item Env:FEANORFS_TEST_CLI_MARKER -ErrorAction SilentlyContinue
    Remove-Item Env:FEANORFS_NO_LAUNCH -ErrorAction SilentlyContinue
    Remove-Item Env:FEANORFS_INSTALLER_TEST_NO_SHORTCUT -ErrorAction SilentlyContinue
    Remove-Item Env:FEANORFS_INSTALLER_TEST_NO_PATH_UPDATE -ErrorAction SilentlyContinue
    Remove-Item Env:FEANORFS_INSTALLER_TEST_FORCE_TRAY_LAUNCH -ErrorAction SilentlyContinue
    if ($null -eq $originalArchitecture) {
        Remove-Item Env:PROCESSOR_ARCHITECTURE -ErrorAction SilentlyContinue
    }
    else {
        $env:PROCESSOR_ARCHITECTURE = $originalArchitecture
    }
}
