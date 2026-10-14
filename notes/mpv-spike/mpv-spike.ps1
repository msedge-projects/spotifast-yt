# mpv-spike.ps1
# Proves the mechanics of controlling mpv over JSON IPC on a Windows named pipe.
# Sessions:
#   1) Main IPC session: property reads, observe_property, loadfile (raw googlevideo URL),
#      time-pos polling, pause toggling, seek, volume, malformed input, stop,
#      client disconnect/reconnect, quit, pipe-after-exit.
#   2) Raw URL with --ytdl=no (does a direct googlevideo URL need yt-dlp at all?)
#   3) mpv built-in ytdl hook: loadfile <youtube watch URL> with no extra flags.
#
# Usage:  powershell -NoProfile -ExecutionPolicy Bypass -File .\mpv-spike.ps1 `
#           -MediaUrlFile .\resolve-g-watch.txt -WatchUrl "https://www.youtube.com/watch?v=dvgZkm1xWPE"
param(
  [string]$MpvExe = "$PSScriptRoot\bin\mpv.exe",
  [string]$MediaUrlFile = "$PSScriptRoot\resolve-g-watch.txt",
  [string]$WatchUrl = "https://www.youtube.com/watch?v=dvgZkm1xWPE",
  [string]$LogFile = "$PSScriptRoot\mpv-ipc-log.txt",
  [int]$Volume = 35
)

$ErrorActionPreference = 'Stop'
trap {
  $m = "FATAL: " + $_.Exception.GetType().FullName + ": " + $_.Exception.Message
  Write-Host $m
  Add-Content -Path $LogFile -Value $m -Encoding UTF8
  foreach ($pp in @($proc, $proc2, $proc3)) { try { if ($pp -and -not $pp.HasExited) { $pp.Kill() } } catch {} }
  exit 1
}
$script:t0 = [System.Diagnostics.Stopwatch]::StartNew()
$script:pending = $null
$script:reader = $null
$script:writer = $null
$script:pipe = $null

function Log([string]$s) {
  $line = "[{0,7} ms] {1}" -f $script:t0.ElapsedMilliseconds, $s
  Write-Host $line
  Add-Content -Path $LogFile -Value $line -Encoding UTF8
}

function LogRaw([string]$s) { Add-Content -Path $LogFile -Value $s -Encoding UTF8 }

function Connect-Pipe([string]$name, [int]$timeoutMs) {
  $deadline = [DateTime]::UtcNow.AddMilliseconds($timeoutMs)
  $sw = [System.Diagnostics.Stopwatch]::StartNew()
  $lastErr = $null
  while ([DateTime]::UtcNow -lt $deadline) {
    $p = New-Object System.IO.Pipes.NamedPipeClientStream(".", $name, [System.IO.Pipes.PipeDirection]::InOut, [System.IO.Pipes.PipeOptions]::None)
    $t = $p.ConnectAsync(1500)
    $ok = $false
    try { $ok = $t.Wait(1500) } catch { $lastErr = $_.Exception }
    if ($ok -and $p.IsConnected) {
      return @{ Pipe = $p; ConnectMs = $sw.ElapsedMilliseconds }
    }
    if (-not $ok -and $null -eq $lastErr) { $lastErr = 'timeout' }
    $p.Dispose()
    Start-Sleep -Milliseconds 100
  }
  throw "connect failed after ${timeoutMs} ms; last error: $($lastErr -replace "`r?`n", ' | ')"
}

function Attach([string]$name, [int]$timeoutMs) {
  $c = Connect-Pipe $name $timeoutMs
  $script:pipe = $c.Pipe
  $script:reader = New-Object System.IO.StreamReader($script:pipe, [System.Text.Encoding]::UTF8)
  $script:writer = New-Object System.IO.StreamWriter($script:pipe, (New-Object System.Text.UTF8Encoding($false)))
  $script:writer.NewLine = "`n"
  $script:writer.AutoFlush = $true
  $script:pending = $null
  Log ("CONNECTED to \\.\pipe\{0} in {1} ms" -f $name, $c.ConnectMs)
}

# timeout-aware line read (at most one outstanding ReadLineAsync)
function Read-TL([int]$timeoutMs) {
  if ($null -eq $script:pending) { $script:pending = $script:reader.ReadLineAsync() }
  if ($script:pending.Wait($timeoutMs)) {
    $line = $script:pending.Result
    $script:pending = $null
    if ($null -eq $line) { return @{ Status = 'eof' } }
    return @{ Status = 'line'; Line = $line }
  }
  return @{ Status = 'timeout' }
}

# read messages until quiet for waitMs; returns array of received JSON strings
function Drain([int]$waitMs) {
  $got = @()
  $deadline = [DateTime]::UtcNow.AddMilliseconds($waitMs)
  while ($true) {
    $remain = [int]([Math]::Max(1, ($deadline - [DateTime]::UtcNow).TotalMilliseconds))
    if ($remain -le 1) { break }
    $r = Read-TL ([Math]::Min($remain, 1000))
    if ($r.Status -eq 'line') { $got += $r.Line; Log ("RECV < " + $r.Line) }
    elseif ($r.Status -eq 'eof') { Log "RECV < <EOF: pipe closed by server>"; break }
    else { break }
  }
  return $got
}

# send raw or object; returns nothing
function Send([object]$payload) {
  $json = $payload
  if ($payload -isnot [string]) { $json = ConvertTo-Json -InputObject $payload -Compress -Depth 6 }
  Log ("SEND > " + $json)
  $script:writer.WriteLine($json)
}

function Req([object]$id, [object[]]$command) {
  $m = [ordered]@{ command = $command }
  if ($null -ne $id) { $m['request_id'] = $id }
  Send $m
}

function GetProp([object]$id, [string]$name) {
  Req $id @('get_property', $name)
}

function WaitForEvent([string]$eventName, [int]$timeoutMs) {
  # returns first received JSON string containing "event":"<name>", or $null
  $deadline = [DateTime]::UtcNow.AddMilliseconds($timeoutMs)
  while ([DateTime]::UtcNow -lt $deadline) {
    $r = Read-TL 250
    if ($r.Status -eq 'line') {
      Log ("RECV < " + $r.Line)
      if ($r.Line -match ('"event"\s*:\s*"' + [regex]::Escape($eventName) + '"')) { return $r.Line }
    } elseif ($r.Status -eq 'eof') { Log "RECV < <EOF: pipe closed by server>"; return $null }
  }
  return $null
}

function ParseLine([string]$s) { return (ConvertFrom-Json -InputObject $s) }

function Disconnect() {
  if ($script:writer) { $script:writer.Dispose() }
  if ($script:reader) { $script:reader.Dispose() }
  if ($script:pipe)   { $script:pipe.Dispose() }
  $script:writer = $null; $script:reader = $null; $script:pipe = $null; $script:pending = $null
  Log "CLIENT: pipe client disconnected/disposed"
}

$rawUrl = (Get-Content -Raw -Path $MediaUrlFile).Trim()
if ($rawUrl -notmatch '^https://') { throw "MediaUrlFile did not contain an URL" }

Log "==== SESSION 1: main IPC session (raw googlevideo URL through mpv default flags) ===="
Log ("mpv: " + $MpvExe)
Log ("media url length: {0} chars; contains expire={1}; contains ip={2}" -f `
  $rawUrl.Length, ($rawUrl -match 'expire='), ($rawUrl -match '&ip='))

$s1out = "$PSScriptRoot\mpv-s1.stdout.txt"
$s1err = "$PSScriptRoot\mpv-s1.stderr.txt"
$s1log = "$PSScriptRoot\mpv-s1.log"
$mpvArgs = @(
  '--idle=yes', '--no-video', '--really-quiet',
  '--input-ipc-server=\\.\pipe\mpv-spike', "--volume=$Volume",
  "--log-file=""$s1log""", '--msg-level=all=info,ipc=v'
)
$swStart = [System.Diagnostics.Stopwatch]::StartNew()
$proc = Start-Process -FilePath $MpvExe -ArgumentList $mpvArgs -PassThru `
        -RedirectStandardOutput $s1out -RedirectStandardError $s1err -WindowStyle Hidden
Log ("START mpv pid=" + $proc.Id)
Attach 'mpv-spike' 8000
$swStart.Stop()
Log ("mpv process start -> pipe connect total: {0} ms" -f $swStart.ElapsedMilliseconds)

# --- r1: idle-active
Req 1 @('get_property', 'idle-active'); Drain 1500 | Out-Null

# --- r2/r3: property reads with nothing loaded
GetProp 2 'time-pos';   Drain 1000 | Out-Null
GetProp 3 'duration';   Drain 1000 | Out-Null

# --- r4: seek while idle (no file)
Req 4 @('seek', 20, 'absolute'); Drain 1000 | Out-Null

# --- r5: observe_property pause (expect immediate property-change + response)
Req 5 @('observe_property', 1, 'pause'); Drain 1500 | Out-Null

# --- r6: volume
GetProp 6 'volume'; Drain 1000 | Out-Null

# --- malformed / unknown input handling
Send 'this is not json {'
Drain 1000 | Out-Null
Req 7 @('bogus_command'); Drain 1000 | Out-Null
GetProp 8 'idle-active'; Drain 1000 | Out-Null

# --- r9: command WITHOUT request_id (is echoing required?)
Send ([ordered]@{ command = @('get_property', 'pause') })
Drain 1000 | Out-Null

# --- loadfile raw URL (default flags; ytdl enabled but googlevideo is not a ytdl site)
$swLoad = [System.Diagnostics.Stopwatch]::StartNew()
Req 10 @('loadfile', $rawUrl, 'replace')
# seek immediately, while still loading/buffering (back-to-back send, no wait)
Req 11 @('seek', 30, 'absolute')
WaitForEvent 'start-file' 5000 | Out-Null
WaitForEvent 'file-loaded' 15000 | Out-Null
$swLoad.Stop()
Log ("loadfile -> file-loaded: {0} ms" -f $swLoad.ElapsedMilliseconds)

# --- poll time-pos (watch null -> number transition)
$firstNonNullMs = -1
for ($i = 0; $i -lt 20; $i++) {
  $id = 20 + $i
  GetProp $id 'time-pos'
  $lines = Drain 400
  foreach ($l in $lines) {
    $o = ParseLine $l
    if ($o.request_id -eq $id) {
      if ($null -ne $o.data -and $firstNonNullMs -lt 0) { $firstNonNullMs = $swLoad.ElapsedMilliseconds }
    }
  }
  Start-Sleep -Milliseconds 250
}
Log ("first non-null time-pos at {0} ms after loadfile" -f $firstNonNullMs)

# --- duration now available
GetProp 60 'duration'; Drain 1000 | Out-Null

# --- pause toggle
Req 61 @('set_property', 'pause', $true);  Drain 1200 | Out-Null
Log "sleeping 2 s while paused"
Start-Sleep -Seconds 2
Req 62 @('set_property', 'pause', $false); Drain 1200 | Out-Null

# --- absolute seek then read time-pos
Req 63 @('seek', 20, 'absolute'); Drain 1500 | Out-Null
Start-Sleep -Milliseconds 800
GetProp 64 'time-pos'; Drain 1000 | Out-Null

# --- volume change
Req 65 @('set_property', 'volume', 60); Drain 1000 | Out-Null
GetProp 66 'volume'; Drain 1000 | Out-Null

Start-Sleep -Seconds 3   # let audio keep playing a bit

# --- stop
Req 67 @('stop'); Drain 2000 | Out-Null
GetProp 68 'idle-active'; Drain 1000 | Out-Null

# --- reconnect test: drop client while mpv keeps running
Disconnect
Start-Sleep -Seconds 2
Log ("mpv still running after client disconnect? " + (-not $proc.HasExited))
Attach 'mpv-spike' 5000
GetProp 69 'idle-active'; Drain 1000 | Out-Null

# --- quit: watch pipe closing
$swQuit = [System.Diagnostics.Stopwatch]::StartNew()
Req 70 @('quit')
$sawEof = $false
while ($swQuit.ElapsedMilliseconds -lt 5000) {
  $r = Read-TL 500
  if ($r.Status -eq 'line') { Log ("RECV < " + $r.Line) }
  elseif ($r.Status -eq 'eof') { $sawEof = $true; Log ("RECV < <EOF at {0} ms after quit>" -f $swQuit.ElapsedMilliseconds); break }
}
Disconnect
$proc.WaitForExit(5000) | Out-Null
Log ("after quit: EOF={0}, mpv hasExited={1}, exitCode={2}" -f $sawEof, $proc.HasExited, $(if ($proc.HasExited) { $proc.ExitCode } else { 'n/a' }))

# --- connect after exit
try {
  $p = New-Object System.IO.Pipes.NamedPipeClientStream(".", "mpv-spike", [System.IO.Pipes.PipeDirection]::InOut, [System.IO.Pipes.PipeOptions]::None)
  $t = $p.ConnectAsync(1500)
  $ok = $false
  try { $ok = $t.Wait(1500) } catch { Log ("POST-EXIT: connect faulted with " + $_.Exception.GetType().FullName + ": " + $_.Exception.Message) }
  if ($ok -and $p.IsConnected) { Log "POST-EXIT: connect unexpectedly succeeded" }
  elseif ($ok) { Log "POST-EXIT: task completed but not connected" }
  $p.Dispose()
} catch {
  Log ("POST-EXIT: connect threw " + $_.Exception.GetType().FullName + ": " + $_.Exception.Message)
}

# ============================================================================
Log ""
Log "==== SESSION 2: raw googlevideo URL with --ytdl=no ===="
$s2out = "$PSScriptRoot\mpv-s2.stdout.txt"; $s2err = "$PSScriptRoot\mpv-s2.stderr.txt"; $s2log = "$PSScriptRoot\mpv-s2.log"
$proc2 = Start-Process -FilePath $MpvExe -ArgumentList @(
  '--idle=yes', '--no-video', '--really-quiet', '--ytdl=no',
  '--input-ipc-server=\\.\pipe\mpv-spike2', "--volume=$Volume",
  "--log-file=""$s2log""", '--msg-level=all=info,ipc=v'
) -PassThru -RedirectStandardOutput $s2out -RedirectStandardError $s2err -WindowStyle Hidden
Attach 'mpv-spike2' 8000
$sw2 = [System.Diagnostics.Stopwatch]::StartNew()
Req 1 @('loadfile', $rawUrl, 'replace'); Drain 500 | Out-Null
WaitForEvent 'file-loaded' 15000 | Out-Null
$tpos2 = $null
for ($i = 0; $i -lt 20; $i++) {
  GetProp (10+$i) 'time-pos'
  foreach ($l in (Drain 400)) { $o = ParseLine $l; if ($o.request_id -eq (10+$i) -and $null -ne $o.data) { $tpos2 = $o.data; break } }
  if ($null -ne $tpos2) { break }
  Start-Sleep -Milliseconds 250
}
$sw2.Stop()
Log ("SESSION2: loadfile -> file-loaded: {0} ms; first non-null time-pos={1} at {2} ms" -f $sw2.ElapsedMilliseconds, $tpos2, $sw2.ElapsedMilliseconds)
Start-Sleep -Seconds 3
Req 99 @('stop'); Drain 1500 | Out-Null
Req 100 @('quit'); Drain 1500 | Out-Null
Disconnect
$proc2.WaitForExit(5000) | Out-Null
Log ("SESSION2: mpv hasExited=" + $proc2.HasExited)

# ============================================================================
Log ""
Log "==== SESSION 3: mpv built-in ytdl hook with plain watch URL ===="
$s3out = "$PSScriptRoot\mpv-s3.stdout.txt"; $s3err = "$PSScriptRoot\mpv-s3.stderr.txt"; $s3log = "$PSScriptRoot\mpv-s3.log"
$proc3 = Start-Process -FilePath $MpvExe -ArgumentList @(
  '--idle=yes', '--no-video', '--really-quiet',
  '--input-ipc-server=\\.\pipe\mpv-spike3', "--volume=$Volume",
  "--log-file=""$s3log""", '--msg-level=all=info,ipc=v'
) -PassThru -RedirectStandardOutput $s3out -RedirectStandardError $s3err -WindowStyle Hidden
Attach 'mpv-spike3' 8000
$sw3 = [System.Diagnostics.Stopwatch]::StartNew()
Req 1 @('loadfile', $WatchUrl, 'replace'); Drain 1000 | Out-Null
WaitForEvent 'file-loaded' 30000 | Out-Null
$swLoaded = $sw3.ElapsedMilliseconds
$tpos3 = $null
for ($i = 0; $i -lt 40; $i++) {
  GetProp (10+$i) 'time-pos'
  foreach ($l in (Drain 400)) { $o = ParseLine $l; if ($o.request_id -eq (10+$i) -and $null -ne $o.data) { $tpos3 = $o.data; break } }
  if ($null -ne $tpos3) { break }
  Start-Sleep -Milliseconds 250
}
$sw3.Stop()
Log ("SESSION3: loadfile -> file-loaded: {0} ms; first non-null time-pos={1} at {2} ms" -f $swLoaded, $tpos3, $sw3.ElapsedMilliseconds)
GetProp 60 'media-title'; Drain 1000 | Out-Null
GetProp 61 'path'; Drain 1000 | Out-Null
Start-Sleep -Seconds 3
Req 99 @('stop'); Drain 1500 | Out-Null
Req 100 @('quit'); Drain 1500 | Out-Null
Disconnect
$proc3.WaitForExit(5000) | Out-Null
Log ("SESSION3: mpv hasExited=" + $proc3.HasExited)

Log ""
Log "==== DONE ===="
Log ("total elapsed: {0} ms" -f $script:t0.ElapsedMilliseconds)
