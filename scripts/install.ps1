# Zuno installer for Windows - install or upgrade, one command either way.
#
# **ASCII only, on purpose.** Windows PowerShell 5.1 reads a .ps1 without a BOM as ANSI, where the
# bytes of an em dash include a curly double quote, which PowerShell reads as a real one: a message
# containing one would end its own string and the file would not parse.
#
#   irm https://raw.githubusercontent.com/aryanjha256/zuno/main/scripts/install.ps1 | iex
#
# **Why a command rather than a download.** The app is not code-signed, and Windows marks a file a
# browser downloaded with the Mark of the Web, so SmartScreen stops it on first launch. PowerShell's
# own download sets no such mark - the Windows counterpart of `install.sh` over curl on macOS.
#
# Installs for the current user, with no admin rights, into %LOCALAPPDATA%\Programs\Zuno; adds a
# Start-menu shortcut, a desktop shortcut on first install, the folder to the user PATH, and an
# entry in Settings > Apps whose Uninstall button removes all of it. Settings, sessions and
# collections live in %APPDATA%\Zuno and are never touched.
#
# **Runs inside the caller's session** under `iex`, so it never calls `exit` - that would close
# the person's terminal. Everything is in one scriptblock and every failure is a `throw`.
#
# Written for Windows PowerShell 5.1 as well as PowerShell 7, because 5.1 is what Windows opens by
# default: `WebClient` rather than `Invoke-WebRequest` (which needs `-UseBasicParsing` there and
# refuses `file://` in 7), TLS 1.2 turned on explicitly, and no `??` or ternaries.
#
# Knobs, as environment variables:
#   ZUNO_VERSION=0.2.4      install that version instead of the latest
#   ZUNO_ASSUME_YES=1       never prompt; required when there is no console to answer
#   ZUNO_FORCE=1            reinstall even when the wanted version is already installed
#   ZUNO_INSTALL_DIR=...    where Zuno goes (default %LOCALAPPDATA%\Programs\Zuno)
#   ZUNO_NO_MODIFY_PATH=1   leave the user PATH alone
#   ZUNO_DOWNLOAD_BASE=...  fetch the assets from here - a mirror, or a local folder when testing

& {
    $ErrorActionPreference = 'Stop'
    # 5.1 redraws its progress bar per chunk, which makes a download many times slower.
    $ProgressPreference = 'SilentlyContinue'
    # 5.1 on an older .NET offers TLS 1.0 first, which GitHub refuses.
    [Net.ServicePointManager]::SecurityProtocol =
        [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    $Repo = 'aryanjha256/zuno'
    $Releases = "https://github.com/$Repo/releases"
    $InstallDir = if ($env:ZUNO_INSTALL_DIR) { $env:ZUNO_INSTALL_DIR } else {
        Join-Path $env:LOCALAPPDATA 'Programs\Zuno'
    }
    $Exe = Join-Path $InstallDir 'zuno.exe'
    $UninstallKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\Zuno'
    $StartMenuLink = Join-Path ([Environment]::GetFolderPath('Programs')) 'Zuno.lnk'
    $DesktopLink = Join-Path ([Environment]::GetFolderPath('Desktop')) 'Zuno.lnk'

    function Say([string]$text) { Write-Host $text }

    # --- versions ----------------------------------------------------------------------

    # Read from the redirect /releases/latest performs, not the API, for `install.sh`'s reasons:
    # the API's unauthenticated limit is per IP, and this needs no JSON.
    function Resolve-Version {
        if ($env:ZUNO_VERSION) { return $env:ZUNO_VERSION.TrimStart('v') }
        # The redirect is *followed* and the final address read back: with redirects off, whether a
        # 302 throws differs between .NET Framework (5.1) and .NET (7), and `ResponseUri` does not.
        $request = [System.Net.WebRequest]::Create("$Releases/latest")
        $request.Method = 'HEAD'
        try {
            $response = $request.GetResponse()
            $location = $response.ResponseUri.AbsoluteUri
            $response.Close()
        } catch {
            throw "Could not reach GitHub to find the latest version. Are you online?"
        }
        if ($location -notmatch '/tag/v?([^/]+)$') {
            throw "Could not read the latest version from '$location'."
        }
        return $Matches[1]
    }

    # What the installed copy says it is: the zip carries `VERSION`, as the Linux tarball does,
    # since there is no package database to ask.
    function Get-InstalledVersion {
        $file = Join-Path $InstallDir 'VERSION'
        if (Test-Path $file) { return (Get-Content $file -TotalCount 1).Trim() }
        return $null
    }

    function Confirm-Step([string]$question) {
        if ($env:ZUNO_ASSUME_YES -eq '1') { return $true }
        # A prompt that cannot be answered must not answer itself - `install.sh`'s rule.
        if (-not [Environment]::UserInteractive) {
            throw "No console to read an answer from. Re-run with `$env:ZUNO_ASSUME_YES = '1'."
        }
        $reply = Read-Host "$question [Y/n]"
        return ($reply -eq '' -or $reply -match '^(y|yes)$')
    }

    # --- download ----------------------------------------------------------------------

    function Get-Base([string]$version) {
        if ($env:ZUNO_DOWNLOAD_BASE) { return $env:ZUNO_DOWNLOAD_BASE.TrimEnd('/') }
        return "$Releases/download/v$version"
    }

    function Save-Url([string]$url, [string]$path) {
        $client = New-Object System.Net.WebClient
        try { $client.DownloadFile($url, $path) } finally { $client.Dispose() }
    }

    # `name -> hash` from `sha256sums.txt`, tolerating the `./` and `*` spellings `install.sh`
    # learnt about the hard way.
    function Read-Sums([string]$path) {
        $sums = @{}
        foreach ($line in Get-Content $path) {
            $fields = $line.Trim() -split '\s+', 2
            if ($fields.Count -eq 2) {
                $name = $fields[1].TrimStart('*')
                if ($name.StartsWith('./')) { $name = $name.Substring(2) }
                $sums[$name] = $fields[0].ToLowerInvariant()
            }
        }
        return $sums
    }

    # --- shortcuts, PATH and the Apps entry --------------------------------------------

    function New-Shortcut([string]$path) {
        $shell = New-Object -ComObject WScript.Shell
        $link = $shell.CreateShortcut($path)
        $link.TargetPath = $Exe
        $link.WorkingDirectory = $InstallDir
        $link.IconLocation = "$Exe,0"
        $link.Description = 'Zuno - a fast, keyboard-first API client'
        $link.Save()
    }

    function Add-ToUserPath {
        if ($env:ZUNO_NO_MODIFY_PATH -eq '1') { return $false }
        $current = [Environment]::GetEnvironmentVariable('Path', 'User')
        $entries = @()
        if ($current) { $entries = $current -split ';' | Where-Object { $_ } }
        if ($entries -contains $InstallDir) { return $false }
        [Environment]::SetEnvironmentVariable('Path', (($entries + $InstallDir) -join ';'), 'User')
        return $true
    }

    # **Generated here, with this install's paths baked in**, rather than shipped in the zip: the
    # uninstaller has to undo exactly what the installer did, so it belongs beside it.
    function Write-Uninstaller {
        $script = @"
# Written by Zuno's installer. Removes the app, its shortcuts, its PATH entry and this Apps entry.
# Your settings and collections in %APPDATA%\Zuno are left where they are.
`$ErrorActionPreference = 'Continue'
`$dir = '$($InstallDir -replace "'", "''")'
if (Get-Process -Name zuno -ErrorAction SilentlyContinue | Where-Object { `$_.Path -like "`$dir\*" }) {
    Write-Host 'Zuno is running. Close it, then uninstall again.'
    exit 1
}
Remove-Item -LiteralPath '$($StartMenuLink -replace "'", "''")' -Force -ErrorAction SilentlyContinue
Remove-Item -LiteralPath '$($DesktopLink -replace "'", "''")' -Force -ErrorAction SilentlyContinue
`$path = [Environment]::GetEnvironmentVariable('Path', 'User')
if (`$path) {
    `$kept = (`$path -split ';' | Where-Object { `$_ -and `$_ -ne `$dir }) -join ';'
    [Environment]::SetEnvironmentVariable('Path', `$kept, 'User')
}
Remove-Item -LiteralPath '$UninstallKey' -Recurse -Force -ErrorAction SilentlyContinue
Remove-Item -LiteralPath `$dir -Recurse -Force
Write-Host 'Zuno is uninstalled.'
"@
        # UTF-8 **with** a BOM, whichever PowerShell runs this: the Apps entry starts it with
        # powershell.exe, which is 5.1 and reads a BOM-less file as ANSI - and a user folder
        # with a non-ASCII name is in every path above. `Set-Content -Encoding UTF8` writes a BOM
        # in 5.1 and none in 7, so the encoding is spelt out.
        [IO.File]::WriteAllText(
            (Join-Path $InstallDir 'uninstall.ps1'), $script, (New-Object Text.UTF8Encoding $true))
    }

    function Register-App([string]$version) {
        $uninstaller = Join-Path $InstallDir 'uninstall.ps1'
        $size = [int]((Get-ChildItem $InstallDir -Recurse -File | Measure-Object Length -Sum).Sum / 1KB)
        New-Item -Path $UninstallKey -Force | Out-Null
        $values = @{
            DisplayName     = 'Zuno'
            DisplayVersion  = $version
            Publisher       = 'Zuno'
            DisplayIcon     = "$Exe,0"
            InstallLocation = $InstallDir
            URLInfoAbout    = "https://github.com/$Repo"
            UninstallString = "powershell.exe -NoProfile -ExecutionPolicy Bypass -File `"$uninstaller`""
        }
        foreach ($name in $values.Keys) {
            New-ItemProperty -Path $UninstallKey -Name $name -Value $values[$name] -PropertyType String -Force | Out-Null
        }
        foreach ($name in 'NoModify', 'NoRepair') {
            New-ItemProperty -Path $UninstallKey -Name $name -Value 1 -PropertyType DWord -Force | Out-Null
        }
        New-ItemProperty -Path $UninstallKey -Name EstimatedSize -Value $size -PropertyType DWord -Force | Out-Null
    }

    # --- install -----------------------------------------------------------------------

    if ([Environment]::OSVersion.Platform -ne 'Win32NT') {
        throw "This installer is for Windows. On Linux and macOS use install.sh."
    }
    # x64 builds only. Windows on ARM runs them through its x64 emulation, so only 32-bit is out.
    if (-not [Environment]::Is64BitOperatingSystem) {
        throw "Zuno needs 64-bit Windows."
    }

    $wanted = Resolve-Version
    $current = Get-InstalledVersion

    $work = Join-Path ([IO.Path]::GetTempPath()) ("zuno-install-" + [guid]::NewGuid())
    New-Item -ItemType Directory -Path $work | Out-Null
    try {
        # The list first, and before any question, because it names the asset: a version with no
        # Windows build is refused up front rather than after agreeing to a downgrade.
        $base = Get-Base $wanted
        $sumsPath = Join-Path $work 'sha256sums.txt'
        try { Save-Url "$base/sha256sums.txt" $sumsPath } catch {
            throw "Could not download the checksums for $wanted. Check that the version exists: $Releases"
        }
        $sums = Read-Sums $sumsPath
        $file = $sums.Keys | Where-Object { $_ -match '^zuno-.*-x86_64-windows\.zip$' } | Select-Object -First 1
        if (-not $file) {
            throw "Zuno $wanted was published before Windows builds existed, so there is none to install."
        }

        if ($current) {
            if ($current -eq $wanted -and $env:ZUNO_FORCE -ne '1') {
                Say "Zuno $current is already the latest. Set `$env:ZUNO_FORCE = '1' to reinstall."
                return
            }
            if ([version]$wanted -lt [version]$current) {
                if (-not (Confirm-Step "Zuno $current is installed. Downgrade to $wanted?")) {
                    throw "Cancelled."
                }
            } else {
                Say "Zuno $current is installed. Updating to $wanted."
            }
        } else {
            Say "Installing Zuno $wanted into $InstallDir - no admin rights needed."
        }

        Say "Downloading $file..."
        $zip = Join-Path $work $file
        Save-Url "$base/$file" $zip
        $actual = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($actual -ne $sums[$file]) { throw "Checksum mismatch on $file - refusing to install." }
        Say 'Checksum verified.'

        $stage = Join-Path $work 'unpacked'
        Expand-Archive -LiteralPath $zip -DestinationPath $stage
        $root = Get-ChildItem -LiteralPath $stage -Directory | Select-Object -First 1
        if (-not $root -or -not (Test-Path (Join-Path $root.FullName 'zuno.exe'))) {
            throw "The archive has no zuno.exe - refusing to install it."
        }

        New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
        # **A running exe cannot be overwritten on Windows, but it can be renamed** - so an update
        # while Zuno is open moves the old one aside and the next run of this cleans it up.
        Remove-Item -LiteralPath "$Exe.old" -Force -ErrorAction SilentlyContinue
        if (Test-Path $Exe) { Move-Item -LiteralPath $Exe -Destination "$Exe.old" -Force }
        Copy-Item -Path (Join-Path $root.FullName '*') -Destination $InstallDir -Recurse -Force
        Remove-Item -LiteralPath "$Exe.old" -Force -ErrorAction SilentlyContinue

        Write-Uninstaller
        New-Shortcut $StartMenuLink
        # Only on a first install: an update must not put back a shortcut someone deleted.
        if (-not $current) { New-Shortcut $DesktopLink }
        $pathAdded = Add-ToUserPath
        Register-App $wanted
    } finally {
        Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
    }

    Say ''
    Say "Zuno $wanted is installed. Open it from the Start menu or the desktop."
    if ($pathAdded) {
        Say "$InstallDir was added to your PATH - new terminals can run: zuno"
    }
}
