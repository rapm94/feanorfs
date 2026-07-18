[CmdletBinding()]
param(
    [Parameter(Mandatory = $true, Position = 0)]
    [string]$Installer,
    [Parameter(Mandatory = $true, Position = 1)]
    [string]$FeanorFSBin,
    [Parameter(Mandatory = $true, Position = 2)]
    [string]$TrayBin,
    [switch]$RequireAuthenticode
)

$ErrorActionPreference = "Stop"
$Installer = (Resolve-Path $Installer).Path
$FeanorFSBin = (Resolve-Path $FeanorFSBin).Path
$TrayBin = (Resolve-Path $TrayBin).Path
$root = Join-Path ([IO.Path]::GetTempPath()) ("feanorfs-installer-smoke-" + [Guid]::NewGuid())
$installDir = Join-Path $root "FeanorFS"
$environmentKey = "HKCU:\Environment"
$originalPath = (Get-ItemProperty -Path $environmentKey -Name Path -ErrorAction SilentlyContinue).Path
$pathExisted = $null -ne $originalPath
New-Item -ItemType Directory -Path $root | Out-Null

function Assert-ValidSignature([string]$Path) {
    $signature = Get-AuthenticodeSignature $Path
    if ($signature.Status -ne "Valid") {
        throw "$(Split-Path -Leaf $Path) does not have a valid Authenticode signature: $($signature.Status)"
    }
}

try {
    if ($RequireAuthenticode) {
        Assert-ValidSignature $Installer
        Assert-ValidSignature $FeanorFSBin
        Assert-ValidSignature $TrayBin
    }

    $installProcess = Start-Process -FilePath $Installer -ArgumentList @(
        "/VERYSILENT",
        "/SUPPRESSMSGBOXES",
        "/NORESTART",
        "/SP-",
        "/DIR=$installDir"
    ) -Wait -PassThru
    if ($installProcess.ExitCode -ne 0) {
        throw "Windows installer exited with status $($installProcess.ExitCode)."
    }

    $installedCli = Join-Path $installDir "feanorfs.exe"
    $installedTray = Join-Path $installDir "feanorfs-tray.exe"
    $uninstaller = Join-Path $installDir "unins000.exe"
    foreach ($file in @($installedCli, $installedTray, $uninstaller, (Join-Path $installDir "LICENSE"))) {
        if (-not (Test-Path -PathType Leaf $file)) {
            throw "Installer payload is missing $(Split-Path -Leaf $file)."
        }
    }
    if ((Get-FileHash $installedCli).Hash -ne (Get-FileHash $FeanorFSBin).Hash -or
        (Get-FileHash $installedTray).Hash -ne (Get-FileHash $TrayBin).Hash) {
        throw "Installed binaries do not match the verified installer inputs."
    }
    if ($RequireAuthenticode) {
        Assert-ValidSignature $installedCli
        Assert-ValidSignature $installedTray
    }

    $userPath = (Get-ItemProperty -Path $environmentKey -Name Path).Path
    if (-not (@($userPath -split ';') -contains $installDir)) {
        throw "Installer did not add the FeanorFS CLI directory to the user PATH."
    }

    $uninstallProcess = Start-Process -FilePath $uninstaller -ArgumentList @(
        "/VERYSILENT",
        "/SUPPRESSMSGBOXES",
        "/NORESTART"
    ) -Wait -PassThru
    if ($uninstallProcess.ExitCode -ne 0) {
        throw "Windows uninstaller exited with status $($uninstallProcess.ExitCode)."
    }
    if ((Test-Path $installedCli) -or (Test-Path $installedTray)) {
        throw "Windows uninstaller left product binaries behind."
    }
    $remainingPath = (Get-ItemProperty -Path $environmentKey -Name Path -ErrorAction SilentlyContinue).Path
    if (@($remainingPath -split ';') -contains $installDir) {
        throw "Windows uninstaller left its CLI directory in the user PATH."
    }

    Write-Host "Windows installer smoke passed: exact CLI/tray payload, PATH integration, signatures, and uninstall are correct."
}
finally {
    if ($pathExisted) {
        Set-ItemProperty -Path $environmentKey -Name Path -Value $originalPath
    }
    else {
        Remove-ItemProperty -Path $environmentKey -Name Path -ErrorAction SilentlyContinue
    }
    Remove-Item -Recurse -Force $root -ErrorAction SilentlyContinue
}
