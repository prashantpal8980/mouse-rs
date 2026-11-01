# B72 check: the "Start at login as administrator" task must start a copy that an
# ordinary (non-administrator) program cannot replace.
#
# Run in a NORMAL PowerShell window (not "Run as administrator"), after turning the
# option on in mouse-rs (Settings > Startup), and again after logging out and back in:
#
#   irm http://192.168.137.2:8000/b72.txt | iex
#
# It only reads, plus one test write into the protected folder that must FAIL. If that
# write unexpectedly succeeds, the test file is deleted again. It changes nothing else.
# The report goes to the drop on :8001. Pure ASCII and iex-safe on purpose.

if ($env:OS -ne 'Windows_NT') {
    Write-Host 'STOP: this check only runs on Windows.' -ForegroundColor Red
    return
}

$server = 'http://192.168.137.2'
$stamp  = Get-Date -Format 'yyyyMMdd-HHmmss'
$report = New-Object System.Text.StringBuilder
$fails  = 0
function Say($m)  { Write-Host "  $m"; [void]$report.AppendLine($m) }
function Pass($m) { Say "PASS  $m" }
function Bad($m)  { Say "FAIL  $m"; $script:fails++ }

[void]$report.AppendLine("B72 check  $stamp  on $env:COMPUTERNAME as $env:USERNAME")

$me = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
if ($me.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Bad 'this window is elevated; run it in a normal PowerShell so the write test means something'
}

$pf     = [Environment]::GetFolderPath('ProgramFiles')
$dir    = Join-Path $pf 'mouse-rs'
$target = Join-Path $dir 'mouse-rs.exe'
Say "expected task target: $target"

# 1. The task exists and starts the Program Files copy, with highest privileges.
$task = $null
try { $task = Get-ScheduledTask -TaskName 'mouse-rs (elevated)' -ErrorAction Stop } catch {}
if (-not $task) {
    Bad 'no task "mouse-rs (elevated)": turn on Start at login as administrator first'
} else {
    $exec = ($task.Actions | Select-Object -First 1).Execute.Trim('"')
    Say "task starts: $exec"
    if ($exec -ieq $target) { Pass 'task points at the Program Files copy' }
    else { Bad 'task does not point at the Program Files copy (an old task? turn the option off and on)' }
    if ($task.Principal.RunLevel -eq 'Highest') { Pass 'task runs with highest privileges' }
    else { Bad "task run level is $($task.Principal.RunLevel)" }
}

# 2. The copy exists.
if (Test-Path $target) {
    $f = Get-Item $target
    Pass "copy present: $($f.Length) bytes, $($f.LastWriteTime)"
    Say  "sha256: $((Get-FileHash $target -Algorithm SHA256).Hash)"
} else {
    Bad "no copy at $target"
}

# 3. This (non-admin) user cannot write there: the heart of B72.
if (Test-Path $dir) {
    $probe = Join-Path $dir ("b72-probe-" + [guid]::NewGuid().ToString('N') + '.tmp')
    $wrote = $false
    try { [IO.File]::WriteAllText($probe, 'x'); $wrote = $true } catch {}
    if ($wrote) {
        Remove-Item $probe -Force -ErrorAction SilentlyContinue
        Bad 'an ordinary program COULD write into the folder (test file removed again)'
    } else {
        Pass 'an ordinary program cannot write into the folder'
    }
    $renamed = $false
    if (Test-Path $target) {
        try { Rename-Item $target 'mouse-rs.exe.b72test' -ErrorAction Stop; $renamed = $true } catch {}
        if ($renamed) {
            Rename-Item (Join-Path $dir 'mouse-rs.exe.b72test') 'mouse-rs.exe' -ErrorAction SilentlyContinue
            Bad 'an ordinary program COULD rename the copy (renamed back)'
        } else {
            Pass 'an ordinary program cannot rename or replace the copy'
        }
    }
    Say 'folder permissions:'
    (icacls $dir) | ForEach-Object { Say "    $_" }
}

# 4. The firewall rule for the copy (read-only query).
$fw = netsh advfirewall firewall show rule name="mouse-rs (elevated)" 2>$null
if ($LASTEXITCODE -eq 0 -and ($fw -match [regex]::Escape($target))) { Pass 'firewall rule for the copy exists' }
else { Say 'NOTE  no firewall rule "mouse-rs (elevated)" naming the copy (only matters when this PC has the mouse)' }

# 5. After a logout/login: which copy is running.
$procs = Get-Process mouse-rs -ErrorAction SilentlyContinue
if ($procs) {
    foreach ($p in $procs) {
        $path = $null
        try { $path = $p.Path } catch {}
        if (-not $path) { $path = '(path hidden: the process is elevated, which is expected for the task copy)' }
        Say "running: pid $($p.Id)  $path"
    }
} else {
    Say 'NOTE  mouse-rs is not running right now'
}

$result = if ($fails -eq 0) { 'ok' } else { 'FAILED' }
Say "RESULT: $result ($fails failed)"
$name = "b72-$($env:COMPUTERNAME)-$stamp-$result.txt"
try {
    Invoke-WebRequest -Uri "$server`:8001/$name" -Method Put -UseBasicParsing `
        -Body $report.ToString() -ContentType 'text/plain' | Out-Null
    Write-Host "  report uploaded as $name"
} catch {
    Write-Host "  could not upload the report ($($_.Exception.Message)); it is printed above."
}
