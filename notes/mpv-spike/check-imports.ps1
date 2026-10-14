param([string]$Exe)
# Minimal PE import-table dump: lists imported DLL names and reports which are unresolvable via LoadLibraryEx.
$bytes = [System.IO.File]::ReadAllBytes($Exe)
$fs = [System.IO.File]::OpenRead($Exe); $br = New-Object System.IO.BinaryReader($fs)
function R16([int]$o){ $fs.Seek($o,'Begin')|Out-Null; $br.ReadUInt16() }
function R32([int]$o){ $fs.Seek($o,'Begin')|Out-Null; $br.ReadUInt32() }
function R64([int]$o){ $fs.Seek($o,'Begin')|Out-Null; $br.ReadUInt64() }

$e_lfanew = R32 0x3C
$sig = R32 $e_lfanew
if ($sig -ne 0x4550) { throw "not PE" }
$coff = $e_lfanew + 4
$numSections = R16 ($coff + 2)
$optSize = R16 ($coff + 16)
$optOff = $coff + 20
$magic = R16 $optOff
$pe32plus = ($magic -eq 0x20b)
$dataDirOff = if ($pe32plus) { $optOff + 112 } else { $optOff + 96 }
$importRva = R32 ($dataDirOff + 8)   # entry 1
$secOff = $optOff + $optSize
$sections = @()
for ($i=0; $i -lt $numSections; $i++) {
  $s = $secOff + 40*$i
  $fs.Seek($s,'Begin')|Out-Null; $nameBytes = $br.ReadBytes(8)
  $name = [System.Text.Encoding]::ASCII.GetString($nameBytes).TrimEnd([char]0)
  $sections += [pscustomobject]@{ Name=$name; VSize=[uint32](R32 ($s+8)); VA=[uint32](R32 ($s+12)); Raw=[uint32](R32 ($s+20)) }
}
function RvaToOff([uint32]$rva){
  foreach ($s in $sections) {
    if ($rva -ge $s.VA -and $rva -lt ($s.VA + [Math]::Max($s.VSize,1))) { return [int]($s.Raw + ($rva - $s.VA)) }
  }
  return -1
}
$off = RvaToOff $importRva
$dlls = @()
while ($true) {
  $nameRva = R32 $off
  $firstThunk = R32 ($off+16)
  if ($nameRva -eq 0 -and $firstThunk -eq 0) { break }
  $no = RvaToOff ([uint32]$nameRva)
  $fs.Seek($no,'Begin')|Out-Null
  $sb = New-Object System.Text.StringBuilder
  while ($true) { $c=$br.ReadByte(); if ($c -eq 0){break}; [void]$sb.Append([char]$c) }
  $dlls += $sb.ToString()
  $off += 20
}
$br.Close(); $fs.Close()
"DLL imports ($($dlls.Count)):"
$dlls | ForEach-Object { $_ }
""
"Resolvability (LoadLibraryExW from exe's dir):"
$h = [NativeDllTest]
Add-Type -Namespace NativeDllTest -Name Dll -MemberDefinition @'
[DllImport("kernel32.dll", SetLastError=true, CharSet=CharSet.Unicode)]
public static extern IntPtr LoadLibraryExW(string lpLibFileName, IntPtr hFile, uint dwFlags);
[DllImport("kernel32.dll")]
public static extern bool FreeLibrary(IntPtr h);
'@
$dir = Split-Path $Exe
foreach ($d in $dlls) {
  $h = [NativeDllTest.Dll]::LoadLibraryExW("$dir\$d", [IntPtr]::Zero, 0)
  $status = if ($h -ne [IntPtr]::Zero) { if ($h -eq [IntPtr](-1)) {"ALREADY_IN_MEMORY"} else { "OK"; [NativeDllTest.Dll]::FreeLibrary($h)|Out-Null } } else { "MISSING" }
  "{0,-40} {1}" -f $d, $status
}
