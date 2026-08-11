$ErrorActionPreference = "Stop"

$root = Join-Path ([IO.Path]::GetTempPath()) ("feanorfs-install-routing-" + [Guid]::NewGuid())
$installDir = Join-Path $root "FeanorFS"
$originalArchitecture = $env:PROCESSOR_ARCHITECTURE
New-Item -ItemType Directory -Path $root | Out-Null

$global:FeanorFSInstallerTestRelease = $null
$global:FeanorFSInstallerTestDownloads = @{}
$global:FeanorFSInstallerTestProcesses = @()
$global:FeanorFSInstallerTestSignatureStatus = @{}
$global:FeanorFSInstallerTestSignerSubjects = @{}
$global:FeanorFSInstallerTestProductVersions = @{}
$global:FeanorFSInstallerTestExitCode = 0
$global:FeanorFSInstallerTestCliVersion = "9.9.9"
$global:FeanorFSInstallerTestCliVersionChecks = 0
$global:FeanorFSInstallerTestCli = [byte[]](1, 2, 3)
$global:FeanorFSInstallerTestTray = [byte[]](4, 5, 6)

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
    $leaf = Split-Path -Leaf $FilePath
    $status = if ($global:FeanorFSInstallerTestSignatureStatus.ContainsKey($leaf)) {
        $global:FeanorFSInstallerTestSignatureStatus[$leaf]
    }
    else {
        "Valid"
    }
    $subject = if ($global:FeanorFSInstallerTestSignerSubjects.ContainsKey($leaf)) {
        $global:FeanorFSInstallerTestSignerSubjects[$leaf]
    }
    else {
        "CN=FeanorFS Test Signer"
    }
    return [pscustomobject]@{
        Status = $status
        SignerCertificate = [pscustomobject]@{ Subject = $subject }
    }
}

function global:Get-Item {
    param([string]$LiteralPath)
    $leaf = Split-Path -Leaf $LiteralPath
    $version = if ($global:FeanorFSInstallerTestProductVersions.ContainsKey($leaf)) {
        $global:FeanorFSInstallerTestProductVersions[$leaf]
    }
    else {
        "9.9.9"
    }
    return [pscustomobject]@{
        VersionInfo = [pscustomobject]@{ ProductVersion = $version }
    }
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
        [object[]]$ArgumentList,
        [switch]$Wait,
        [switch]$PassThru,
        [switch]$NoNewWindow,
        [string]$RedirectStandardOutput,
        [string]$RedirectStandardError,
        [string]$WorkingDirectory
    )
    if ((Split-Path -Leaf $FilePath) -eq "feanorfs.exe" -and
        $ArgumentList.Count -eq 1 -and $ArgumentList[0] -eq "--version") {
        $global:FeanorFSInstallerTestCliVersionChecks++
        [IO.File]::WriteAllText(
            $RedirectStandardOutput,
            "feanorfs $($global:FeanorFSInstallerTestCliVersion)`n"
        )
        [IO.File]::WriteAllText($RedirectStandardError, "")
        return [pscustomobject]@{ ExitCode = 0 }
    }
    $global:FeanorFSInstallerTestProcesses += [pscustomobject]@{
        FilePath = $FilePath
        ArgumentList = @($ArgumentList)
        Wait = [bool]$Wait
        PassThru = [bool]$PassThru
    }
    if ((Split-Path -Leaf $FilePath) -eq "FeanorFS-windows-x86_64-setup.exe") {
        if ($global:FeanorFSInstallerTestExitCode -eq 0) {
            New-Item -ItemType Directory -Force -Path $installDir | Out-Null
            [IO.File]::WriteAllBytes((Join-Path $installDir "feanorfs.exe"), $global:FeanorFSInstallerTestCli)
            [IO.File]::WriteAllBytes((Join-Path $installDir "feanorfs-tray.exe"), $global:FeanorFSInstallerTestTray)
        }
        return [pscustomobject]@{ ExitCode = $global:FeanorFSInstallerTestExitCode }
    }
    return [pscustomobject]@{ ExitCode = 0 }
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
    $env:PROCESSOR_ARCHITECTURE = "AMD64"

    $global:FeanorFSInstallerTestRelease = [pscustomobject]@{
        tag_name = "v9.9.9-rc.1"
        assets = @()
    }
    $failedClosed = $false
    try {
        & "$PSScriptRoot/install.ps1" *> (Join-Path $root "unstable-tag.log")
    }
    catch {
        $failedClosed = $_.Exception.Message -like "*canonical stable version tag*"
    }
    if (-not $failedClosed -or $global:FeanorFSInstallerTestProcesses.Count -ne 0) {
        throw "A noncanonical release tag did not fail before download or execution."
    }

    $global:FeanorFSInstallerTestRelease = [pscustomobject]@{
        tag_name = "v9.9.9"
        assets = @((New-Asset "feanorfs-client-installer.ps1"))
    }
    $failedClosed = $false
    try {
        & "$PSScriptRoot/install.ps1" *> (Join-Path $root "legacy.log")
    }
    catch {
        $failedClosed = $_.Exception.Message -like "*canonical signed Windows setup EXE*"
    }
    if (-not $failedClosed -or $global:FeanorFSInstallerTestProcesses.Count -ne 0) {
        throw "Legacy Windows release did not fail closed before executing a remote script."
    }

    $setupName = "FeanorFS-windows-x86_64-setup.exe"
    $setupBytes = [Text.Encoding]::UTF8.GetBytes("signed-setup-fixture")
    $global:FeanorFSInstallerTestDownloads["https://example.invalid/$setupName"] = $setupBytes
    $global:FeanorFSInstallerTestRelease = [pscustomobject]@{
        tag_name = "v9.9.9"
        assets = @((New-Asset $setupName))
    }
    $failedClosed = $false
    try {
        & "$PSScriptRoot/install.ps1" *> (Join-Path $root "missing-checksum.log")
    }
    catch {
        $failedClosed = $_.Exception.Message -like "*without its checksum*"
    }
    if (-not $failedClosed -or $global:FeanorFSInstallerTestProcesses.Count -ne 0) {
        throw "Windows setup without a checksum did not fail closed."
    }

    $global:FeanorFSInstallerTestDownloads["https://example.invalid/$setupName.sha256"] =
        [Text.Encoding]::ASCII.GetBytes(("0" * 64) + "  $setupName`n")
    $global:FeanorFSInstallerTestRelease = [pscustomobject]@{
        tag_name = "v9.9.9"
        assets = @((New-Asset $setupName), (New-Asset "$setupName.sha256"))
    }
    $failedClosed = $false
    try {
        & "$PSScriptRoot/install.ps1" *> (Join-Path $root "bad-checksum.log")
    }
    catch {
        $failedClosed = $_.Exception.Message -like "*checksum does not match*"
    }
    if (-not $failedClosed -or $global:FeanorFSInstallerTestProcesses.Count -ne 0) {
        throw "Windows setup with a bad checksum did not fail closed."
    }

    $setupFixture = Join-Path $root $setupName
    [IO.File]::WriteAllBytes($setupFixture, $setupBytes)
    $setupHash = (Get-FileHash -Algorithm SHA256 $setupFixture).Hash.ToLowerInvariant()
    $global:FeanorFSInstallerTestDownloads["https://example.invalid/$setupName.sha256"] =
        [Text.Encoding]::ASCII.GetBytes("$setupHash  $setupName`n")
    $global:FeanorFSInstallerTestSignatureStatus[$setupName] = "NotSigned"
    $failedClosed = $false
    try {
        & "$PSScriptRoot/install.ps1" *> (Join-Path $root "unsigned-setup.log")
    }
    catch {
        $failedClosed = $_.Exception.Message -like "*failed Authenticode verification*"
    }
    if (-not $failedClosed -or $global:FeanorFSInstallerTestProcesses.Count -ne 0) {
        throw "Unsigned Windows setup did not fail before execution."
    }

    $global:FeanorFSInstallerTestSignatureStatus[$setupName] = "Valid"
    $global:FeanorFSInstallerTestProductVersions[$setupName] = "9.9.8"
    $global:FeanorFSInstallerTestProcesses = @()
    $failedClosed = $false
    try {
        & "$PSScriptRoot/install.ps1" *> (Join-Path $root "rollback-setup.log")
    }
    catch {
        $failedClosed = $_.Exception.Message -like "*ProductVersion*does not match release*"
    }
    if (-not $failedClosed -or $global:FeanorFSInstallerTestProcesses.Count -ne 0) {
        throw "A validly signed rollback setup was not rejected before execution."
    }

    $global:FeanorFSInstallerTestProductVersions[$setupName] = "9.9.9"
    $global:FeanorFSInstallerTestSignerSubjects[$setupName] = "CN=Unexpected Signer"
    $env:FEANORFS_WINDOWS_SIGNER_SUBJECTS = "CN=Approved FeanorFS Signer"
    $failedClosed = $false
    try {
        & "$PSScriptRoot/install.ps1" *> (Join-Path $root "wrong-signer.log")
    }
    catch {
        $failedClosed = $_.Exception.Message -like "*not signed by an approved FeanorFS identity*"
    }
    if (-not $failedClosed -or $global:FeanorFSInstallerTestProcesses.Count -ne 0) {
        throw "A valid signature from an unapproved signer was not rejected."
    }
    $global:FeanorFSInstallerTestSignerSubjects[$setupName] = "CN=Approved FeanorFS Signer"
    $global:FeanorFSInstallerTestSignerSubjects["feanorfs.exe"] = "CN=Approved FeanorFS Signer"
    $global:FeanorFSInstallerTestSignerSubjects["feanorfs-tray.exe"] = "CN=Approved FeanorFS Signer"

    $global:FeanorFSInstallerTestSignatureStatus["feanorfs.exe"] = "NotSigned"
    $global:FeanorFSInstallerTestCliVersionChecks = 0
    $global:FeanorFSInstallerTestProcesses = @()
    $failedClosed = $false
    try {
        & "$PSScriptRoot/install.ps1" *> (Join-Path $root "unsigned-installed-cli.log")
    }
    catch {
        $failedClosed = $_.Exception.Message -like "*Installed feanorfs.exe failed Authenticode verification*"
    }
    if (-not $failedClosed -or
        $global:FeanorFSInstallerTestProcesses.Count -ne 1 -or
        $global:FeanorFSInstallerTestCliVersionChecks -ne 0) {
        throw "An unsigned installed CLI was not rejected before its version was executed."
    }
    $global:FeanorFSInstallerTestSignatureStatus["feanorfs.exe"] = "Valid"

    $global:FeanorFSInstallerTestCliVersion = "9.9.8"
    $global:FeanorFSInstallerTestCliVersionChecks = 0
    $global:FeanorFSInstallerTestProcesses = @()
    $failedClosed = $false
    try {
        & "$PSScriptRoot/install.ps1" *> (Join-Path $root "rollback-installed-cli.log")
    }
    catch {
        $failedClosed = $_.Exception.Message -like "*Installed feanorfs.exe version*does not match release*"
    }
    if (-not $failedClosed -or
        $global:FeanorFSInstallerTestProcesses.Count -ne 1 -or
        $global:FeanorFSInstallerTestCliVersionChecks -ne 1) {
        throw "A mismatched installed CLI version was not rejected after setup."
    }
    $global:FeanorFSInstallerTestCliVersion = "9.9.9"
    $global:FeanorFSInstallerTestCliVersionChecks = 0
    $global:FeanorFSInstallerTestProcesses = @()
    $env:FEANORFS_INSTALLER_TEST_FORCE_TRAY_LAUNCH = "1"
    & "$PSScriptRoot/install.ps1" *> (Join-Path $root "desktop-success.log")
    $installedCli = Join-Path $installDir "feanorfs.exe"
    $installedTray = Join-Path $installDir "feanorfs-tray.exe"
    if (-not (Test-Path -PathType Leaf $installedCli) -or -not (Test-Path -PathType Leaf $installedTray)) {
        throw "Canonical Windows setup did not produce the expected installed payload."
    }
    if ($global:FeanorFSInstallerTestProcesses.Count -ne 2 -or
        $global:FeanorFSInstallerTestCliVersionChecks -ne 1) {
        throw "Canonical Windows install did not execute one setup, one CLI version probe, and one tray launch."
    }
    $setupProcess = $global:FeanorFSInstallerTestProcesses[0]
    $trayProcess = $global:FeanorFSInstallerTestProcesses[1]
    $expectedArguments = @(
        "/VERYSILENT",
        "/SUPPRESSMSGBOXES",
        "/NORESTART",
        "/SP-",
        "/DIR=`"$installDir`""
    )
    if (
        (Split-Path -Leaf $setupProcess.FilePath) -ne $setupName -or
        -not $setupProcess.Wait -or
        -not $setupProcess.PassThru -or
        (Compare-Object -ReferenceObject $expectedArguments -DifferenceObject @($setupProcess.ArgumentList)) -or
        $trayProcess.FilePath -ne $installedTray -or
        @($trayProcess.ArgumentList).Count -ne 1 -or
        $trayProcess.ArgumentList[0] -ne "--first-run"
    ) {
        throw "PowerShell routing did not invoke the canonical setup and tray with exact arguments."
    }
    $successLog = Get-Content -Raw (Join-Path $root "desktop-success.log")
    if ($successLog -notlike "*PATH, Start-menu, and uninstall integration*" -or
        $successLog -notlike "*FeanorFS is now in your system tray*") {
        throw "Canonical Windows installation did not report native integration and tray-first onboarding."
    }

    $global:FeanorFSInstallerTestCliVersionChecks = 0
    $global:FeanorFSInstallerTestProcesses = @()
    $env:FEANORFS_NO_LAUNCH = "1"
    & "$PSScriptRoot/install.ps1" *> (Join-Path $root "desktop-no-launch.log")
    if (
        $global:FeanorFSInstallerTestProcesses.Count -ne 1 -or
        $global:FeanorFSInstallerTestCliVersionChecks -ne 1 -or
        (Split-Path -Leaf $global:FeanorFSInstallerTestProcesses[0].FilePath) -ne $setupName
    ) {
        throw "Windows no-launch path did not run only the canonical setup."
    }
    $noLaunchLog = Get-Content -Raw (Join-Path $root "desktop-no-launch.log")
    if ($noLaunchLog -notlike "*Headless setup: feanorfs start*") {
        throw "Windows no-launch installer path did not provide the headless setup command."
    }

    Write-Host "Windows installer routing passed: legacy execution is blocked and canonical setup verification is tray-first."
}
finally {
    Remove-Item function:global:Invoke-RestMethod -ErrorAction SilentlyContinue
    Remove-Item function:global:Invoke-WebRequest -ErrorAction SilentlyContinue
    Remove-Item function:global:Get-AuthenticodeSignature -ErrorAction SilentlyContinue
    Remove-Item function:global:Get-Item -ErrorAction SilentlyContinue
    Remove-Item function:global:Get-Process -ErrorAction SilentlyContinue
    Remove-Item function:global:Start-Process -ErrorAction SilentlyContinue
    Remove-Variable -Name FeanorFSInstallerTestRelease -Scope Global -ErrorAction SilentlyContinue
    Remove-Variable -Name FeanorFSInstallerTestDownloads -Scope Global -ErrorAction SilentlyContinue
    Remove-Variable -Name FeanorFSInstallerTestProcesses -Scope Global -ErrorAction SilentlyContinue
    Remove-Variable -Name FeanorFSInstallerTestSignatureStatus -Scope Global -ErrorAction SilentlyContinue
    Remove-Variable -Name FeanorFSInstallerTestSignerSubjects -Scope Global -ErrorAction SilentlyContinue
    Remove-Variable -Name FeanorFSInstallerTestProductVersions -Scope Global -ErrorAction SilentlyContinue
    Remove-Variable -Name FeanorFSInstallerTestExitCode -Scope Global -ErrorAction SilentlyContinue
    Remove-Variable -Name FeanorFSInstallerTestCliVersion -Scope Global -ErrorAction SilentlyContinue
    Remove-Variable -Name FeanorFSInstallerTestCliVersionChecks -Scope Global -ErrorAction SilentlyContinue
    Remove-Variable -Name FeanorFSInstallerTestCli -Scope Global -ErrorAction SilentlyContinue
    Remove-Variable -Name FeanorFSInstallerTestTray -Scope Global -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force $root -ErrorAction SilentlyContinue
    Remove-Item Env:FEANORFS_RELEASE_API -ErrorAction SilentlyContinue
    Remove-Item Env:FEANORFS_INSTALL_DIR -ErrorAction SilentlyContinue
    Remove-Item Env:FEANORFS_NO_LAUNCH -ErrorAction SilentlyContinue
    Remove-Item Env:FEANORFS_INSTALLER_TEST_FORCE_TRAY_LAUNCH -ErrorAction SilentlyContinue
    Remove-Item Env:FEANORFS_WINDOWS_SIGNER_SUBJECTS -ErrorAction SilentlyContinue
    if ($null -eq $originalArchitecture) {
        Remove-Item Env:PROCESSOR_ARCHITECTURE -ErrorAction SilentlyContinue
    }
    else {
        $env:PROCESSOR_ARCHITECTURE = $originalArchitecture
    }
}
