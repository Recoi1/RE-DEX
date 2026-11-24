# Self-check of check-release.ps1's deny list: a fake release folder with the player's own files planted where the
# allow-list would let them through (toolchain\include\ takes anything) must fail, naming each; the clean one passes.
# Run: powershell -File tools\release\test-check-release.ps1 (exits 1 on a miss).
$ErrorActionPreference = 'Stop'
$check = Join-Path $PSScriptRoot 'check-release.ps1'
$r = Join-Path ([IO.Path]::GetTempPath()) "redex-check-release-$PID"
if (Test-Path $r) { Remove-Item $r -Recurse -Force }
New-Item -ItemType Directory -Force "$r\toolchain\include" | Out-Null
Set-Content "$r\redex.exe" 'x'
Set-Content "$r\toolchain\include\stdio.h" 'x'
& powershell -NoProfile -File $check -Release $r *> $null
if ($LASTEXITCODE) { Write-Error 'the clean folder failed the check' -ErrorAction Continue; exit 1 }
# saves, states, card pictures, characters and account files: never shipped
$planted = 'characters.json', 'characters.json.bad-1700000000000', 'characters.json.new', 'card-00000000000000a1.png',
    'slot.state', 'redex-id', 'library.json', 'settings.toml', 'reports.jsonl', 'accounts\0123456789abcdef0123456789abcdef.bin.tmp',
    'characters\00000000000000a1\user\savedata\0004000000055D00\main', 'characters\00000000000000a1\backups\0004000000055D00\main-1700000000',
    'characters.json.new-4242', 'girl.4242-1.part', 'preview\x-usa-v0.2.0\notes.txt'
foreach ($p in $planted) {
    $f = Join-Path "$r\toolchain\include" $p
    New-Item -ItemType Directory -Force (Split-Path $f) | Out-Null
    Set-Content $f 'x'
}
$ErrorActionPreference = 'Continue'
$out = & powershell -NoProfile -File $check -Release $r 2>&1 | ForEach-Object { "$_" } | Out-String -Width 1000
$code = $LASTEXITCODE
Remove-Item $r -Recurse -Force
$missed = $planted | Where-Object { -not $out.Contains("toolchain\include\$_ ") }
if ($code -ne 1 -or $missed) { Write-Error "check-release let through: $($missed -join ', ') (exit $code)" -ErrorAction Continue; exit 1 }
# a test build (cargo --features mp-tests, or sim): its --help names the MP test harness, and it never ships
foreach ($exe in 'redex.exe', 'redex-world.exe') {
    New-Item -ItemType Directory -Force $r | Out-Null
    Set-Content "$r\$exe" "MZ...  MP-TESTS BUILD (the MP test harness): [--dev-id]"
    $out = & powershell -NoProfile -File $check -Release $r 2>&1 | ForEach-Object { "$_" } | Out-String -Width 1000
    $code = $LASTEXITCODE
    Remove-Item $r -Recurse -Force
    if ($code -ne 1 -or -not ($out -replace '\s+', ' ').Contains("$exe (built with mp-tests")) { Write-Error "check-release let an mp-tests $exe through (exit $code)" -ErrorAction Continue; exit 1 }
}
"check-release refuses all $($planted.Count) planted files and mp-tests builds of redex and redex-world"
exit 0
