# mouse-rs standing installer. Quickest, in an elevated PowerShell:
#   irm http://192.168.137.2:8000/u.txt | iex
# Keep this file ASCII and iex-safe (no param(), exit, $PSScriptRoot or $MyInvocation): the
# one-liner above runs it as text. Or, as the assistant prompt does it:
#   $f="$env:TEMP\mouse-rs-update.ps1"; iwr http://192.168.137.2:8000/update.ps1 -UseBasicParsing -OutFile $f; Unblock-File $f; powershell -NoProfile -ExecutionPolicy Bypass -Command "Set-ExecutionPolicy -Scope Process Bypass -Force -ErrorAction SilentlyContinue; Invoke-Expression ([IO.File]::ReadAllText('$f'))"
#
# The point of this script is that the owner keeps using mouse-rs while it updates. So a
# running instance is replaced by the new build and started again straight away, with the
# same command line it had. If the new build does not stay up, the previous exe is put back
# and started. Nothing is left stopped.
#
# In order, and what each step leaves behind if it fails:
#   0. Runs only on Windows, from an elevated shell (mouse-rs runs elevated, so a normal
#      shell can neither stop it nor replace its exe). Nothing is changed.
#   1. Reads the expected size and SHA256 from CHECKSUM.txt on the same server.
#   2. Downloads the exe to mouse-rs.exe.new and checks size and hash. On a mismatch it
#      deletes the download and stops. The installed exe and the running app are untouched.
#   3. Records the running instance's executable path and command line, so it can be started
#      again the same way. If none is running, it records that and will not start one.
#   4. Stops it and proves it stopped.
#   5. Keeps the previous exe as mouse-rs.exe.bak, moves the verified download into place, and
#      checks the installed file's size and hash.
#   6. Starts the new build with the recorded command line, and waits to see it stay up. If
#      it does not, it restores mouse-rs.exe.bak and starts the previous build instead.
#   7. Writes a report and uploads it to the report drop on :8001.
#
# It does not touch %USERPROFILE%\.mouse_share (config, shared secret, logs) and sends no
# keyboard or mouse input of any kind.

$ErrorActionPreference = 'Stop'

$server  = 'http://192.168.137.2'
$dir     = Join-Path $env:USERPROFILE 'mouse-rs'
$exe     = Join-Path $dir 'mouse-rs.exe'
$new     = Join-Path $dir 'mouse-rs.exe.new'
$bak     = Join-Path $dir 'mouse-rs.exe.bak'
$stamp   = Get-Date -Format 'yyyyMMdd-HHmmss'
$report  = New-Object System.Text.StringBuilder

function Say($m) {
    Write-Host "  $m"
    [void]$report.AppendLine($m)
}

function Upload-Report($ok) {
    $name = "update-$($env:COMPUTERNAME)-$stamp-$(if ($ok) {'ok'} else {'FAILED'}).txt"
    try {
        Invoke-WebRequest -Uri "$server`:8001/$name" -Method Put -UseBasicParsing `
            -Body $report.ToString() -ContentType 'text/plain' | Out-Null
        Write-Host "  report uploaded as $name"
    } catch {
        Write-Host "  could not upload the report ($($_.Exception.Message)); it is printed above."
    }
}

function Fail($m) {
    Say "STOP: $m"
    Upload-Report $false
    throw $m
}

function Hash-Of($path) {
    (Get-FileHash $path -Algorithm SHA256).Hash
}

# The arguments that follow the executable in a command line such as
#   "C:\Users\x\mouse-rs\mouse-rs.exe" gui
# or the unquoted form. Returns '' when there are none.
function Get-ArgsOf($commandLine) {
    $cl = $commandLine.Trim()
    if ($cl.StartsWith('"')) {
        $end = $cl.IndexOf('"', 1)
        if ($end -lt 0) { return '' }
        return $cl.Substring($end + 1).Trim()
    }
    $space = $cl.IndexOf(' ')
    if ($space -lt 0) { return '' }
    return $cl.Substring($space + 1).Trim()
}

# Start one instance of an exe with an argument string, in its own folder, and return the
# process. Start-Process keeps the elevation of this (elevated) shell.
function Start-Mouse($path, $argString) {
    $params = @{ FilePath = $path; WorkingDirectory = (Split-Path $path); PassThru = $true }
    if ($argString) { $params.ArgumentList = $argString }
    Start-Process @params
}

# Give a freshly started build five seconds, so one that crashes straight after starting is
# caught, then return the process if it is still running from $path. Returns $null if not.
function Wait-Stays-Up($path) {
    Start-Sleep -Seconds 5
    $match = Get-Process mouse-rs -ErrorAction SilentlyContinue |
        Where-Object { try { $_.Path -eq $path } catch { $false } }
    return $match | Select-Object -First 1
}

Write-Host ''
Write-Host 'mouse-rs update' -ForegroundColor Cyan
[void]$report.AppendLine("mouse-rs update  $stamp  on $env:COMPUTERNAME")

# --- 0. Windows only, and only from an elevated shell ------------------------
# The stop step matches every process called mouse-rs. On any other machine that is the
# real app, so this refuses to run anywhere but Windows.
if ($env:OS -ne 'Windows_NT') {
    Write-Host 'STOP: this installer only runs on Windows. Nothing has been changed.' -ForegroundColor Red
    return
}
$me = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
if (-not $me.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Write-Host ''
    Write-Host 'STOP: this shell is not elevated. Nothing has been changed.' -ForegroundColor Red
    Write-Host '  Close it, press Win+X, choose "Terminal (Admin)", then run the same line again.'
    return
}
Say 'shell is elevated: yes'

if (-not (Test-Path $dir)) { Fail "no mouse-rs folder at $dir" }

# --- 1. expected size and hash, from the server ------------------------------
$sums = (Invoke-WebRequest -Uri "$server`:8000/CHECKSUM.txt" -UseBasicParsing).Content
$wantSize = [int64]([regex]::Match($sums, 'mouse-rs\.exe\s+(\d+)\s+bytes').Groups[1].Value)
$wantHash = [regex]::Match($sums, 'SHA256\s+([0-9A-Fa-f]{64})').Groups[1].Value.ToUpper()
if ($wantSize -le 0 -or $wantHash.Length -ne 64) { Fail 'CHECKSUM.txt on the server did not parse' }
Say "server build: $wantSize bytes, SHA256 $wantHash"

# --- 2. download to a side file, and verify it before anything else ----------
if (Test-Path $new) { Remove-Item $new -Force }
Invoke-WebRequest -Uri "$server`:8000/mouse-rs.exe" -OutFile $new -UseBasicParsing
$gotSize = (Get-Item $new).Length
$gotHash = Hash-Of $new
if ($gotSize -ne $wantSize -or $gotHash -ne $wantHash) {
    Remove-Item $new -Force
    Fail "download is wrong (size $gotSize, SHA256 $gotHash). The installed exe and the running app were not touched."
}
Say 'download verified: size and SHA256 match the server'

# --- 3. what is installed and running now -------------------------------------
if (Test-Path $exe) {
    $cur = Get-Item $exe
    Say "installed before: $($cur.Length) bytes, modified $($cur.LastWriteTime), SHA256 $(Hash-Of $exe)"
} else {
    Say 'installed before: nothing (first install)'
}
$running = @(Get-CimInstance Win32_Process -Filter "Name = 'mouse-rs.exe'" -ErrorAction SilentlyContinue)
$restartArgs = $null
if ($running.Count -gt 0) {
    $first = $running[0]
    $restartArgs = Get-ArgsOf $first.CommandLine
    Say "running before: pid $($running.ProcessId -join ', ') from $($first.ExecutablePath)"
    Say "restarting it with the same arguments: '$restartArgs'"
} else {
    Say 'running before: no. It was not running, so it will not be started.'
}

# --- 4. stop it, and prove it stopped ----------------------------------------
if ($running.Count -gt 0) {
    Say "stopping mouse-rs (pid $($running.ProcessId -join ', '))"
    $running | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
    Start-Sleep -Seconds 2
}
if (Get-Process mouse-rs -ErrorAction SilentlyContinue) {
    Fail 'mouse-rs is still running, so its exe cannot be replaced. Nothing was replaced.'
}
Say 'mouse-rs is stopped'

# --- 5. keep the old exe, move the verified one into place, check it ---------
if (Test-Path $exe) {
    Copy-Item $exe $bak -Force
    Say 'previous exe kept as mouse-rs.exe.bak'
}
Move-Item $new $exe -Force
$instSize = (Get-Item $exe).Length
$instHash = Hash-Of $exe
if ($instSize -ne $wantSize -or $instHash -ne $wantHash) {
    Fail "the installed exe does not match (size $instSize, SHA256 $instHash). mouse-rs.exe.bak holds the previous build."
}
Say "installed: $instSize bytes, SHA256 $instHash, verified"

# --- 6. start it again, and check it stays up --------------------------------
if ($restartArgs -ne $null) {
    Say 'starting the new build'
    Start-Mouse $exe $restartArgs | Out-Null
    $up = Wait-Stays-Up $exe
    if ($up) {
        Say "running after: pid $($up.Id) from $exe, still up after 5 seconds. It reconnects to the peer on its own if auto-connect is on."
    } else {
        Say 'the new build did not stay up. Putting the previous build back.'
        if (Test-Path $bak) {
            Copy-Item $bak $exe -Force
            Say 'restored the previous exe from mouse-rs.exe.bak'
            Start-Mouse $exe $restartArgs | Out-Null
            $back = Wait-Stays-Up $exe
            if ($back) { Say "previous build is running again (pid $($back.Id))" }
            else { Say 'the previous build did not start either. Needs attention.' }
        } else {
            Say 'no previous exe to restore'
        }
        Fail 'the new build did not stay up, so it was rolled back.'
    }
} else {
    Say 'nothing to restart. The new build is in place and not started.'
}

Upload-Report $true
Write-Host ''
Write-Host 'Done. The new build is installed and verified.' -ForegroundColor Green
