# diag1.ps1 - minimal mpv IPC probe with tracing between every step
$ErrorActionPreference = 'Stop'
function T($s){ "[{0}] {1}" -f (Get-Date -Format 'HH:mm:ss.fff'), $s }

$mpv = "$PSScriptRoot\bin\mpv.exe"
$proc = Start-Process -FilePath $mpv -ArgumentList @('--idle=yes','--no-video','--really-quiet','--input-ipc-server=\\.\pipe\mpv-diag','--volume=35') -PassThru
T "mpv started pid=$($proc.Id)"

$pipe = New-Object System.IO.Pipes.NamedPipeClientStream(".", "mpv-diag", [System.IO.Pipes.PipeDirection]::InOut, [System.IO.Pipes.PipeOptions]::None)
T "connecting..."
$t = $pipe.ConnectAsync(5000); $ok = $false
try { $ok = $t.Wait(5000) } catch { T "connect fault: $($_.Exception.Message)" }
T "connected=$ok isConnected=$($pipe.IsConnected)"
$reader = New-Object System.IO.StreamReader($pipe, [System.Text.Encoding]::UTF8)
$writer = New-Object System.IO.StreamWriter($pipe, (New-Object System.Text.UTF8Encoding($false)))
$writer.NewLine = "`n"; $writer.AutoFlush = $true

T "sending r1"
$writer.WriteLine('{"command":["get_property","idle-active"],"request_id":1}')
T "sent r1"
$task = $reader.ReadLineAsync()
T "issued ReadLineAsync, waiting 3000ms"
$got = $false; try { $got = $task.Wait(3000) } catch { T "read fault: $($_.Exception.Message)" }
T "wait returned $got"
if ($got) { T ("r1 response: " + $task.Result) }

T "sending r2"
$writer.WriteLine('{"command":["get_property","time-pos"],"request_id":2}')
T "sent r2"
$task2 = $reader.ReadLineAsync()
T "issued ReadLineAsync, waiting 3000ms"
$got2 = $false; try { $got2 = $task2.Wait(3000) } catch { T "read fault: $($_.Exception.Message)" }
T "wait returned $got2"
if ($got2) { T ("r2 response: " + $task2.Result) }

T "sending r3 (duration)"
$writer.WriteLine('{"command":["get_property","duration"],"request_id":3}')
$task3 = $reader.ReadLineAsync()
$got3 = $false; try { $got3 = $task3.Wait(3000) } catch { T "read fault: $($_.Exception.Message)" }
T "wait returned $got3"
if ($got3) { T ("r3 response: " + $task3.Result) }

T "done; killing mpv"
if (-not $proc.HasExited) { $proc.Kill() }
$proc.WaitForExit(3000) | Out-Null
T "mpv hasExited=$($proc.HasExited)"
