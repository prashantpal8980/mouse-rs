# mouse-rs rollback to the 2026-09-04 build (the one that ran for five weeks).
# Served from the Kali box; run it with one line, from an ADMINISTRATOR PowerShell:
#   iex (iwr http://192.168.137.2:8000/rollback.ps1 -UseBasicParsing).Content
#
# Written as a file on purpose: pasting a multi-line block into the PowerShell console
# silently drops parts of it, because a blank line ends the >> continuation prompt.

$ErrorActionPreference = 'Stop'

$dir  = Join-Path $env:USERPROFILE 'mouse-rs'
$exe  = Join-Path $dir 'mouse-rs.exe'
$new  = Join-Path $dir 'mouse-rs.exe.new'
$bak  = Join-Path $dir 'mouse-rs.exe.bak'
$url  = 'http://192.168.137.2:8000/mouse-rs.exe'
$want = '5076363C55AA0E7B16CE43AD11BDDCC4B3BFE42DDC9EBFE13003CBE54F9011F1'
$size = 12080128

function Say($m) { Write-Host "  $m" }

Write-Host ''
Write-Host 'mouse-rs rollback -> 2026-09-04 build' -ForegroundColor Cyan

# --- 0. elevation, checked up front -----------------------------------------
# mouse-rs is running elevated, so a normal shell can neither stop it nor replace
# the file it holds open. Checked here so it fails before anything is touched.
$me = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
if (-not $me.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Write-Host ''
    Write-Host 'STOP: this shell is not elevated.' -ForegroundColor Red
    Say 'Close it, press Win+X, choose "Terminal (Admin)" or'
    Say '"Windows PowerShell (Admin)", then paste the same one-liner.'
    Say 'Nothing on disk has been changed.'
    return
}
Say 'shell is elevated: yes'

if (-not (Test-Path $dir)) { throw "no mouse-rs folder at $dir" }

# --- 1. stop it, and prove it stopped ---------------------------------------
$p = Get-Process mouse-rs -ErrorAction SilentlyContinue
if ($p) {
    Say "stopping mouse-rs (pid $($p.Id -join ', '))"
    $p | Stop-Process -Force
    Start-Sleep -Seconds 2
} else {
    Say 'mouse-rs was not running'
}
if (Get-Process mouse-rs -ErrorAction SilentlyContinue) {
    throw 'mouse-rs is STILL running, so its exe cannot be replaced. Nothing changed.'
}
Say 'stopped'

# --- 2. get the build, and verify it before touching anything ---------------
if (Test-Path $new) {
    Say 'using the copy already downloaded'
} else {
    Say 'downloading'
    Invoke-WebRequest -Uri $url -OutFile $new -UseBasicParsing
}
$len = (Get-Item $new).Length
$sha = (Get-FileHash $new -Algorithm SHA256).Hash
Say "size   $len"
Say "sha256 $sha"
if ($len -ne $size -or $sha -ne $want) {
    Remove-Item $new -Force
    throw 'MISMATCH - deleted the download. Your existing exe is untouched.'
}
Say 'verified against the expected hash'

# --- 3. replace, keeping the build being thrown away ------------------------
if (Test-Path $bak) { Remove-Item $bak -Force }
if (Test-Path $exe) { Move-Item $exe $bak -Force; Say 'previous exe kept as mouse-rs.exe.bak' }
Move-Item $new $exe -Force
if ((Get-FileHash $exe -Algorithm SHA256).Hash -ne $want) {
    throw 'the installed file is not the one that was verified.'
}
Say 'installed'

# --- 4. start it; this shell is elevated, so no second UAC prompt -----------
Start-Process -FilePath $exe
Start-Sleep -Seconds 8
$r = Get-Process mouse-rs -ErrorAction SilentlyContinue
if ($r) {
    Write-Host ''
    Write-Host "DONE - mouse-rs is running elevated (pid $($r.Id -join ', '))" -ForegroundColor Green
    Say 'Now try Ctrl+Space and Alt+Tab across to Kali.'
} else {
    Write-Host ''
    Write-Host 'Installed, but it is not running. Start it from the Start menu.' -ForegroundColor Yellow
}
Write-Host ''
