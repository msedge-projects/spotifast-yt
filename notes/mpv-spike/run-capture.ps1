param(
  [Parameter(Mandatory=$true)][string]$JsonArgsFile,   # file containing JSON array: [exe, arg1, arg2, ...]
  [string]$Out,
  [string]$ErrFile
)
$JsonArgs = Get-Content -Raw -Path $JsonArgsFile
$argv = ConvertFrom-Json -InputObject $JsonArgs
if ($argv -isnot [array]) { $argv = @($argv) }
$exe = [string]$argv[0]
$rest = @()
if ($argv.Count -gt 1) { $rest = @($argv[1..($argv.Count-1)] | ForEach-Object { [string]$_ }) }
$sw = [System.Diagnostics.Stopwatch]::StartNew()
if ($ErrFile) { $stdout = & $exe @rest 2> $ErrFile } else { $stdout = & $exe @rest 2>&1 }
$code = $LASTEXITCODE
$sw.Stop()
$pass = ($stdout | Out-String)
if ($Out) { Set-Content -Path $Out -Value $pass -Encoding UTF8 }
"--- exit=$code  elapsed_ms=$($sw.ElapsedMilliseconds)  stdout_length=$($pass.Length)"
"--- stdout (control chars visible):"
($pass -replace "`r", '\r' -replace "`n", '\n' -replace "`t", '\t')
if ($ErrFile -and (Test-Path $ErrFile)) {
  $e = Get-Content $ErrFile -Raw
  "--- stderr ($($e.Length) chars):"
  ($e -replace "`r", '\r' -replace "`n", '\n')
}
