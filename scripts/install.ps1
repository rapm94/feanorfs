# FeanorFS Windows product installer. Downloads only the canonical native
# setup EXE, verifies its release checksum and Authenticode signature, and lets
# that signed product own PATH, Start-menu, uninstall, and payload integration.
[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$repository = if ($env:FEANORFS_REPOSITORY) { $env:FEANORFS_REPOSITORY } else { "rapm94/feanorfs" }
$releaseApi = if ($env:FEANORFS_RELEASE_API) { $env:FEANORFS_RELEASE_API } else { "https://api.github.com/repos/$repository/releases/latest" }
$defaultInstallDir = if ($env:LOCALAPPDATA) {
    Join-Path $env:LOCALAPPDATA "Programs\FeanorFS"
}
else {
    Join-Path $HOME "AppData\Local\Programs\FeanorFS"
}
$installDir = if ($env:FEANORFS_INSTALL_DIR) { $env:FEANORFS_INSTALL_DIR } else { $defaultInstallDir }

function Get-ReleaseAsset([object]$release, [string]$name) {
    return $release.assets | Where-Object { $_.name -eq $name } | Select-Object -First 1
}

function Save-Url([string]$url, [string]$path) {
    Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $path
}

function Get-ProductVersion([string]$path) {
    $value = (Get-Item -LiteralPath $path).VersionInfo.ProductVersion
    if ([string]::IsNullOrWhiteSpace($value)) {
        throw "$(Split-Path -Leaf $path) does not contain a Windows ProductVersion."
    }
    return $value.Trim()
}

function Assert-TrustedSignature([string]$path, [string]$label) {
    $signature = Get-AuthenticodeSignature $path
    if ($signature.Status -ne "Valid") {
        throw "$label failed Authenticode verification: $($signature.Status)."
    }
    if ($env:FEANORFS_WINDOWS_SIGNER_SUBJECTS) {
        $approved = @(
            $env:FEANORFS_WINDOWS_SIGNER_SUBJECTS.Split(';') |
                ForEach-Object { $_.Trim() } |
                Where-Object { -not [string]::IsNullOrWhiteSpace($_) }
        )
        if ($approved.Count -eq 0) {
            throw "The FeanorFS Windows signer policy is configured but empty."
        }
        $subject = $signature.SignerCertificate.Subject
        if ([string]::IsNullOrWhiteSpace($subject) -or -not ($approved -ccontains $subject)) {
            throw "$label was not signed by an approved FeanorFS identity."
        }
    }
    return $signature
}

function Get-InstalledCliVersion([string]$path) {
    $stdoutPath = [IO.Path]::GetTempFileName()
    $stderrPath = [IO.Path]::GetTempFileName()
    try {
        $process = Start-Process -FilePath $path -ArgumentList @("--version") `
            -Wait -PassThru -NoNewWindow `
            -RedirectStandardOutput $stdoutPath -RedirectStandardError $stderrPath
        if ($process.ExitCode -ne 0) {
            throw "Installed feanorfs.exe could not report its version (status $($process.ExitCode))."
        }
        $text = (Get-Content -Raw $stdoutPath).Trim()
        if ($text -notmatch '^feanorfs ([0-9]+\.[0-9]+\.[0-9]+)$') {
            throw "Installed feanorfs.exe reported an invalid version."
        }
        return $Matches[1]
    }
    finally {
        Remove-Item -Force $stdoutPath, $stderrPath -ErrorAction SilentlyContinue
    }
}

Write-Host "Fetching latest FeanorFS release..."
$release = Invoke-RestMethod -UseBasicParsing -Uri $releaseApi
$version = $release.tag_name
if ([string]::IsNullOrWhiteSpace($version)) {
    throw "Could not determine the latest FeanorFS version."
}
if ($version -notmatch '^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$') {
    throw "The latest FeanorFS release does not use a canonical stable version tag."
}
$releaseVersion = $version.Substring(1)

$assetName = "FeanorFS-windows-x86_64-setup.exe"
$installerAsset = Get-ReleaseAsset $release $assetName
$architecture = $env:PROCESSOR_ARCHITECTURE
$supportsDesktop = $architecture -in @("AMD64", "x86_64")
$installedTray = $null

if (-not $supportsDesktop) {
    throw "Release $version does not contain a trusted Windows installer for $architecture; no legacy script was executed."
}
if (-not $installerAsset) {
    throw "Release $version does not contain the canonical signed Windows setup EXE; no legacy script was executed."
}

$checksumAsset = Get-ReleaseAsset $release "$assetName.sha256"
if (-not $checksumAsset) {
    throw "Release $version lists the Windows setup EXE without its checksum."
}

$temp = Join-Path ([IO.Path]::GetTempPath()) ("feanorfs-install-" + [Guid]::NewGuid())
New-Item -ItemType Directory -Path $temp | Out-Null
try {
    $setup = Join-Path $temp $assetName
    $checksumFile = "$setup.sha256"
    Write-Host "Downloading signed FeanorFS $version for Windows (CLI + system tray)..."
    Save-Url $installerAsset.browser_download_url $setup
    Save-Url $checksumAsset.browser_download_url $checksumFile

    $checksumLine = (Get-Content -Raw $checksumFile).Trim()
    if ($checksumLine -notmatch '^([0-9a-fA-F]{64})\s+FeanorFS-windows-x86_64-setup\.exe$') {
        throw "The Windows setup checksum file has an invalid format."
    }
    $actualHash = (Get-FileHash -Algorithm SHA256 $setup).Hash
    if ($actualHash -ne $Matches[1]) {
        throw "The Windows setup checksum does not match."
    }

    $null = Assert-TrustedSignature $setup "The Windows setup EXE"
    $setupVersion = Get-ProductVersion $setup
    if ($setupVersion -cne $releaseVersion) {
        throw "The Windows setup ProductVersion $setupVersion does not match release $version."
    }

    $arguments = @(
        "/VERYSILENT",
        "/SUPPRESSMSGBOXES",
        "/NORESTART",
        "/SP-",
        "/DIR=`"$installDir`""
    )
    $installProcess = Start-Process -FilePath $setup -ArgumentList $arguments -Wait -PassThru
    if ($installProcess.ExitCode -ne 0) {
        throw "The signed Windows installer exited with status $($installProcess.ExitCode)."
    }

    $installedCli = Join-Path $installDir "feanorfs.exe"
    $installedTray = Join-Path $installDir "feanorfs-tray.exe"
    foreach ($binary in @($installedCli, $installedTray)) {
        if (-not (Test-Path -PathType Leaf $binary)) {
            throw "The signed Windows installer did not install $(Split-Path -Leaf $binary)."
        }
        $null = Assert-TrustedSignature $binary "Installed $(Split-Path -Leaf $binary)"
    }
    $installedVersion = Get-InstalledCliVersion $installedCli
    if ($installedVersion -cne $releaseVersion) {
        throw "Installed feanorfs.exe version $installedVersion does not match release $version."
    }
}
finally {
    Remove-Item -Recurse -Force $temp -ErrorAction SilentlyContinue
}

Write-Host "Installed the signed FeanorFS setup product to $installDir with PATH, Start-menu, and uninstall integration."

Write-Host ""
if ($installedTray) {
    $canLaunch =
        [Environment]::UserInteractive -or
        $env:FEANORFS_INSTALLER_TEST_FORCE_TRAY_LAUNCH -eq "1"
    if ($env:FEANORFS_NO_LAUNCH -eq "1" -or -not $canLaunch) {
        Write-Host "Open FeanorFS from the Start menu to start mirroring a folder."
        Write-Host "Headless setup: feanorfs start C:\path\to\project"
    }
    else {
        try {
            $alreadyRunning = @(Get-Process -Name "feanorfs-tray" -ErrorAction SilentlyContinue).Count -gt 0
            if (-not $alreadyRunning) {
                Start-Process -FilePath $installedTray -ArgumentList @("--first-run") -WorkingDirectory $HOME
            }
            Write-Host "FeanorFS is now in your system tray."
            Write-Host "Choose Start Mirroring a Folder... to begin; no terminal setup is required."
        }
        catch {
            Write-Warning "FeanorFS was installed, but the system tray could not open: $($_.Exception.Message)"
            Write-Host "Open FeanorFS from the Start menu, or run: feanorfs start C:\path\to\project"
        }
    }
}
else {
    Write-Host "First computer:  feanorfs start C:\path\to\project"
    Write-Host "Another computer: feanorfs start <pair-code-or-invite> C:\path\to\project"
}
