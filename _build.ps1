# Patchbay build helper (Windows PowerShell 5.1).
# WHY: this machine has TWO VS installs. `vswhere -latest` picks "VS 18 Community",
# which is INCOMPLETE (no VC headers/include, no lib\x64, no vcvarsall.bat) -> cannot
# compile any C/C++ (Tauri pulls cc-based crates like vswhom-sys). The COMPLETE toolchain
# is the separate "VS 2022 BuildTools". We capture its full env from vcvars64.bat.
# Usage:  powershell -File _build.ps1            (debug)
#         powershell -File _build.ps1 -Release   (release, for the size gate)
# Output + exit code -> src-tauri\build.log
param([switch]$Release)
$vcvars = "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
cmd /c "`"$vcvars`" >nul 2>&1 && set" | ForEach-Object {
  if ($_ -match '^([^=]+)=(.*)$') { Set-Item "Env:\$($matches[1])" $matches[2] }
}
Set-Location (Join-Path $PSScriptRoot "src-tauri")
if ($Release) { cmd /c "cargo build --release > build.log 2>&1" }
else          { cmd /c "cargo build > build.log 2>&1" }
"CARGO_EXIT: $LASTEXITCODE" | Out-File -Append -Encoding utf8 build.log
