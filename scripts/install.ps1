# FeanorFS Windows product installer. Installs the signed CLI and system tray
# from one checksummed release bundle, with a truthful CLI-only fallback for
# historical releases that predate the desktop package.
[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$repository = if ($env:FEANORFS_REPOSITORY) { $env:FEANORFS_REPOSITORY } else { "rapm94/feanorfs" }
$releaseApi = if ($env:FEANORFS_RELEASE_API) { $env:FEANORFS_RELEASE_API } else { "https://api.github.com/repos/$repository/releases/latest" }
$installDir = if ($env:FEANORFS_INSTALL_DIR) { $env:FEANORFS_INSTALL_DIR } else { Join-Path $HOME ".local\bin" }

function Get-ReleaseAsset([object]$release, [string]$name) {
    return $release.assets | Where-Object { $_.name -eq $name } | Select-Object -First 1
}

function Save-Url([string]$url, [string]$path) {
    Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $path
}

Write-Host "Fetching latest FeanorFS release..."
$release = Invoke-RestMethod -UseBasicParsing -Uri $releaseApi
$version = $release.tag_name
if ([string]::IsNullOrWhiteSpace($version)) {
    throw "Could not determine the latest FeanorFS version."
}

$assetName = "FeanorFS-windows-x86_64.zip"
$bundle = Get-ReleaseAsset $release $assetName
$architecture = $env:PROCESSOR_ARCHITECTURE
$supportsDesktop = $architecture -in @("AMD64", "x86_64")
$installedTray = $null

if ($bundle -and $supportsDesktop) {
    $checksumAsset = Get-ReleaseAsset $release "$assetName.sha256"
    if (-not $checksumAsset) {
        throw "Release $version lists the Windows desktop bundle without its checksum."
    }

    $temp = Join-Path ([IO.Path]::GetTempPath()) ("feanorfs-install-" + [Guid]::NewGuid())
    New-Item -ItemType Directory -Path $temp | Out-Null
    try {
        $archive = Join-Path $temp $assetName
        $checksumFile = "$archive.sha256"
        Write-Host "Downloading signed FeanorFS $version for Windows (CLI + system tray)..."
        Save-Url $bundle.browser_download_url $archive
        Save-Url $checksumAsset.browser_download_url $checksumFile

        $checksumLine = (Get-Content -Raw $checksumFile).Trim()
        if ($checksumLine -notmatch '^([0-9a-fA-F]{64})\s+FeanorFS-windows-x86_64\.zip$') {
            throw "The Windows bundle checksum file has an invalid format."
        }
        $actualHash = (Get-FileHash -Algorithm SHA256 $archive).Hash
        if ($actualHash -ne $Matches[1]) {
            throw "The Windows bundle checksum does not match."
        }

        $expanded = Join-Path $temp "expanded"
        Expand-Archive -Path $archive -DestinationPath $expanded
        $archiveFiles = @(
            Get-ChildItem -File -Recurse $expanded |
                ForEach-Object {
                    $_.FullName.Substring($expanded.Length).TrimStart('\').Replace('\', '/')
                } |
                Sort-Object
        )
        $expectedFiles = @("feanorfs-tray.exe", "feanorfs.exe")
        if (Compare-Object -ReferenceObject $expectedFiles -DifferenceObject $archiveFiles) {
            throw "The Windows desktop bundle contains unexpected files."
        }
        $cli = Join-Path $expanded "feanorfs.exe"
        $tray = Join-Path $expanded "feanorfs-tray.exe"
        foreach ($binary in @($cli, $tray)) {
            if (-not (Test-Path -PathType Leaf $binary)) {
                throw "The Windows bundle is missing $(Split-Path -Leaf $binary)."
            }
            $signature = Get-AuthenticodeSignature $binary
            if ($signature.Status -ne "Valid") {
                throw "$(Split-Path -Leaf $binary) failed Authenticode verification: $($signature.Status)."
            }
        }

        New-Item -ItemType Directory -Force -Path $installDir | Out-Null
        Copy-Item -Force $cli (Join-Path $installDir "feanorfs.exe")
        Copy-Item -Force $tray (Join-Path $installDir "feanorfs-tray.exe")
        $installedTray = Join-Path $installDir "feanorfs-tray.exe"
    }
    finally {
        Remove-Item -Recurse -Force $temp -ErrorAction SilentlyContinue
    }

    if ($env:FEANORFS_INSTALLER_TEST_NO_PATH_UPDATE -ne "1") {
        $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
        $pathParts = @($userPath -split ';' | Where-Object { $_ })
        if (-not ($pathParts | Where-Object { $_.TrimEnd('\') -ieq $installDir.TrimEnd('\') })) {
            $newPath = (@($pathParts) + $installDir) -join ';'
            [Environment]::SetEnvironmentVariable("Path", $newPath, "User")
        }
        if (-not (($env:Path -split ';') | Where-Object { $_.TrimEnd('\') -ieq $installDir.TrimEnd('\') })) {
            $env:Path = "$installDir;$env:Path"
        }
    }

    $programs = [Environment]::GetFolderPath("Programs")
    if ($env:FEANORFS_INSTALLER_TEST_NO_SHORTCUT -ne "1" -and -not [string]::IsNullOrWhiteSpace($programs)) {
        New-Item -ItemType Directory -Force -Path $programs | Out-Null
        $shell = New-Object -ComObject WScript.Shell
        try {
            $shortcut = $shell.CreateShortcut((Join-Path $programs "FeanorFS.lnk"))
            $shortcut.TargetPath = Join-Path $installDir "feanorfs-tray.exe"
            $shortcut.WorkingDirectory = $HOME
            $shortcut.Description = "FeanorFS encrypted working-directory sync"
            $shortcut.IconLocation = Join-Path $installDir "feanorfs-tray.exe"
            $shortcut.Save()
        }
        finally {
            [void][Runtime.InteropServices.Marshal]::FinalReleaseComObject($shell)
        }
    }

    Write-Host "Installed signed feanorfs.exe and feanorfs-tray.exe to $installDir with a Start menu shortcut."
}
else {
    $installerAsset = Get-ReleaseAsset $release "feanorfs-client-installer.ps1"
    if (-not $installerAsset) {
        throw "Release $version has neither a compatible desktop bundle nor the CLI installer."
    }
    $tempInstaller = Join-Path ([IO.Path]::GetTempPath()) ("feanorfs-cli-" + [Guid]::NewGuid() + ".ps1")
    try {
        Save-Url $installerAsset.browser_download_url $tempInstaller
        $env:FEANORFS_CLIENT_INSTALL_DIR = $installDir
        & $tempInstaller
        if (-not $?) {
            throw "The FeanorFS CLI installer failed."
        }
    }
    finally {
        Remove-Item -Force $tempInstaller -ErrorAction SilentlyContinue
    }
    Write-Warning "This release does not contain a signed tray for $architecture; the CLI was installed."
}

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
