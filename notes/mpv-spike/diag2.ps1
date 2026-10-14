# diag2.ps1 - replicate the exact function pattern that hung, with granular traces
$ErrorActionPreference = 'Stop'
$script:pending = $null
function T($s){ "[{0}] {1}" -f (Get-Date -Format 'HH:mm:ss.fff'), $s }

function Read-TL([int]$timeoutMs) {
  if ($null -eq $script:pending) { T "Read-TL: issuing ReadLineAsync"; $script:pending = $script:reader.ReadLineAsync() }
  T ("Read-TL: Wait({0})" -f $timeoutMs)
  if ($script:pending.Wait($timeoutMs)) {
    $line = $script:pending.Result
    $script:pending = $null
    if ($null -eq $line) { T "Read-TL: EOF"; return @{ Status = 'eof' } }
    T "Read-TL: got line"
    return @{ Status = 'line'; Line = $line }
  }
  T "Read-TL: timeout"
  return @{ Status = 'timeout' }
}

function Drain([int]$waitMs) {
  $got = @()
  $deadline = [DateTime]::UtcNow.AddMilliseconds($waitMs)
  while ($true) {
    $remain = [int]([Math]::Max(1, ($deadline - [DateTime]::UtcNow).TotalMilliseconds))
    if ($remain -le 1) { break }
    T ("Drain: remain={0}" -f $remain)
    $r = Read-TL ([Math]::Min($remain, 1000))
    if ($r.Status -eq 'line') { $got += $r.Line; T ("RECV < " + $r.Line) }
    elseif ($r.Status -eq 'eof') { T "RECV < EOF"; break }
    else { break }
  }
  return $got
}

function Send([object]$payload) {
  $json = $payload
  if ($payload -isnot [string]) { $json = ConvertTo-Json -InputObject $payload -Compress -Depth 6 }
  T ("SEND > " + $json)
  $script:writer.WriteLine($json)
  T "SEND: write returned"
}

$mpv = "$PSScriptRoot\bin\mpv.exe"
$proc = Start-Process -FilePath $mpv -ArgumentList @('--idle=yes','--no-video','--really-quiet','--input-ipc-server=\\.\pipe\mpv-diag2','--volume=35') -PassThru
T "mpv started pid=$($proc.Id)"
$pipe = New-Object System.IO.Pipes.NamedPipeClientStream(".", "mpv-diag2", [System.IO.Pipes.PipeDirection]::InOut, [System.IO.Pipes.PipeOptions]::None)
$t = $pipe.ConnectAsync(5000); $ok = $false; try { $ok = $t.Wait(5000) } catch { T "connect fault: $($_.Exception.Message)" }
T "connected=$ok"
$script:reader = New-Object System.IO.StreamReader($pipe, [System.Text.Encoding]::UTF8)
$script:writer = New-Object System.IO.StreamWriter($pipe, (New-Object System.Text.UTF8Encoding($false)))
$script:writer.NewLine = "`n"; $script:writer.AutoFlush = $true

T "== r1: get_property idle-active"
Send ([ordered]@{ command = @('get_property','idle-active'); request_id = 1 })
Drain 1500 | Out-Null

T "== r2: get_property time-pos"
Send ([ordered]@{ command = @('get_property','time-pos'); request_id = 2 })
Drain 1000 | Out-Null

T "== r3: get_property duration"
Send ([ordered]@{ command = @('get_property','duration'); request_id = 3 })
Drain 1000 | Out-Null

T "== r4: seek while idle"
Send ([ordered]@{ command = @('seek', 20, 'absolute'); request_id = 4 })
Drain 1000 | Out-Null

T "== r5: observe_property pause"
Send ([ordered]@{ command = @('observe_property', 1, 'pause'); request_id = 5 })
Drain 1500 | Out-Null

T "killing mpv"
if (-not $proc.HasExited) { $proc.Kill() }
$proc.WaitForExit(3000) | Out-Null
T "done"
