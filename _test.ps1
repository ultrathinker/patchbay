$vcvars = "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
cmd /c "`"$vcvars`" >nul 2>&1 && set" | ForEach-Object {
  if ($_ -match '^([^=]+)=(.*)$') { Set-Item "Env:\$($matches[1])" $matches[2] }
}
Set-Location "C:\VS_PROJECTS\_NonWork\Patchbay\src-tauri"
cmd /c "cargo test > test.log 2>&1"
"CARGO_TEST_EXIT: $LASTEXITCODE" | Out-File -Append -Encoding utf8 test.log
