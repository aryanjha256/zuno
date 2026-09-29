# CI only: drive scripts/install.ps1 end to end against fake releases built here - the Windows
# twin of test-tarball-install.sh. Run once under PowerShell 7 and once under Windows PowerShell
# 5.1, because 5.1 is what Windows opens by default and the two differ in exactly the places an
# installer touches: web requests, file encodings and error handling.
#
# **Run as `irm | iex` runs it**: the script's text through Invoke-Expression, in this session,
# so a stray `exit` in the installer would end this test the way it would close a user's terminal.
#
# ASCII only, for install.ps1's reason.
#
#   pwsh -File test-windows-install.ps1 -Repo C:\path\to\repo
param([Parameter(Mandatory = $true)][string]$Repo)

$ErrorActionPreference = 'Stop'
$work = Join-Path ([IO.Path]::GetTempPath()) ("zuno-test-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $work | Out-Null

$dir = Join-Path $env:LOCALAPPDATA 'Programs\Zuno'
$exe = Join-Path $dir 'zuno.exe'
$key = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\Zuno'
$startMenu = Join-Path ([Environment]::GetFolderPath('Programs')) 'Zuno.lnk'
$desktop = Join-Path ([Environment]::GetFolderPath('Desktop')) 'Zuno.lnk'

function Fail([string]$message) {
    Write-Host "::error::$message"
    exit 1
}

# One fake release: the real layout, a stand-in exe, a checksum list. Any real program serves as
# the stand-in, since nothing launches it; what is checked is where it lands and what is registered.
function New-Release([string]$version) {
    $release = Join-Path $work "rel-$version"
    $name = "zuno-$version-x86_64-windows"
    $stage = Join-Path $release "stage\$name"
    New-Item -ItemType Directory -Path $stage -Force | Out-Null
    Copy-Item (Join-Path $env:WINDIR 'System32\whoami.exe') (Join-Path $stage 'zuno.exe')
    Set-Content -LiteralPath (Join-Path $stage 'VERSION') -Value $version -Encoding ASCII
    Copy-Item (Join-Path $Repo 'LICENSE') $stage
    $zip = Join-Path $release "$name.zip"
    Compress-Archive -Path $stage -DestinationPath $zip
    $hash = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant()
    Set-Content -LiteralPath (Join-Path $release 'sha256sums.txt') -Value "$hash  $name.zip" -Encoding ASCII
}

# The installer's output, or the text of what it threw.
function Invoke-Installer([string]$version) {
    $env:ZUNO_VERSION = $version
    $env:ZUNO_DOWNLOAD_BASE = ([Uri](Join-Path $work "rel-$version")).AbsoluteUri
    $env:ZUNO_ASSUME_YES = '1'
    $text = Get-Content -Raw -LiteralPath (Join-Path $Repo 'scripts\install.ps1')
    try {
        return (Invoke-Expression $text *>&1 | Out-String)
    } catch {
        return "THREW: $($_.Exception.Message)"
    }
}

function Get-UserPath { [Environment]::GetEnvironmentVariable('Path', 'User') -split ';' }

New-Release '0.0.1'
New-Release '0.0.2'

# --- a fresh install ---------------------------------------------------------------------
$out = Invoke-Installer '0.0.1'
Write-Host $out
if ($out -match 'THREW') { Fail "the install failed: $out" }
if ($out -notmatch 'Checksum verified') { Fail 'the checksum was not verified' }
if (-not (Test-Path $exe)) { Fail "no zuno.exe at $exe" }
if ((Get-Content (Join-Path $dir 'VERSION')).Trim() -ne '0.0.1') { Fail 'VERSION is not 0.0.1' }
if (-not (Test-Path $startMenu)) { Fail 'no Start-menu shortcut' }
if (-not (Test-Path $desktop)) { Fail 'no desktop shortcut on a first install' }
if ((Get-UserPath) -notcontains $dir) { Fail 'the install folder is not on the user PATH' }
$entry = Get-ItemProperty -Path $key
if ($entry.DisplayVersion -ne '0.0.1') { Fail "Apps entry says $($entry.DisplayVersion)" }
if ($entry.InstallLocation -ne $dir) { Fail "Apps entry points at $($entry.InstallLocation)" }
Write-Host 'ok: installed, with shortcuts, PATH and an Apps entry'

# --- the same version again is a no-op ---------------------------------------------------
$out = Invoke-Installer '0.0.1'
if ($out -notmatch 'already the latest') { Fail "re-running did not report an up-to-date install: $out" }
Write-Host 'ok: re-run is a no-op'

# --- an update, which must not restore a desktop shortcut someone deleted ----------------
Remove-Item -LiteralPath $desktop
$out = Invoke-Installer '0.0.2'
if ($out -match 'THREW') { Fail "the update failed: $out" }
if ((Get-Content (Join-Path $dir 'VERSION')).Trim() -ne '0.0.2') { Fail 'the update did not land' }
if (Test-Path $desktop) { Fail 'the update put back a deleted desktop shortcut' }
if ((Get-ItemProperty -Path $key).DisplayVersion -ne '0.0.2') { Fail 'the Apps entry was not updated' }
if (@(Get-UserPath | Where-Object { $_ -eq $dir }).Count -ne 1) { Fail 'the PATH entry was duplicated' }
Write-Host 'ok: updated in place'

# --- a tampered download is refused -----------------------------------------------------
$sums = Join-Path $work 'rel-0.0.1\sha256sums.txt'
$line = (Get-Content $sums).Trim()
Set-Content -LiteralPath $sums -Value ('0' * 64 + $line.Substring(64)) -Encoding ASCII
$env:ZUNO_FORCE = '1'
$out = Invoke-Installer '0.0.1'
Remove-Item Env:\ZUNO_FORCE
if ($out -notmatch 'Checksum mismatch') { Fail "a bad checksum was not refused: $out" }
if ((Get-Content (Join-Path $dir 'VERSION')).Trim() -ne '0.0.2') { Fail 'a refused install changed files' }
Write-Host 'ok: a checksum mismatch is refused'

# --- uninstall, exactly as Settings > Apps runs it ---------------------------------------
# Through Start-Process, which hands cmd the command line verbatim: passed as an argument, the
# quotes inside the string would be re-quoted and it would not be the command Settings runs.
$uninstall = (Get-ItemProperty -Path $key).UninstallString
$process = Start-Process -FilePath cmd.exe -ArgumentList "/d /c $uninstall" -Wait -PassThru -NoNewWindow
if ($process.ExitCode -ne 0) { Fail "the uninstaller exited $($process.ExitCode)" }
if (Test-Path $dir) { Fail 'the install folder is still there' }
if (Test-Path $startMenu) { Fail 'the Start-menu shortcut is still there' }
if (Test-Path $key) { Fail 'the Apps entry is still there' }
if ((Get-UserPath) -contains $dir) { Fail 'the PATH entry is still there' }
Write-Host 'ok: the uninstaller removes everything it added'

Remove-Item -LiteralPath $work -Recurse -Force
Remove-Item Env:\ZUNO_VERSION, Env:\ZUNO_DOWNLOAD_BASE, Env:\ZUNO_ASSUME_YES
Write-Host "ok: install.ps1 works under PowerShell $($PSVersionTable.PSVersion)"
