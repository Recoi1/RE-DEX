# The guard on a release folder: every file must be on the allow-list of what ships, and nothing may look like game
# data, a save, a save state, a build product or a picture of the game, even where the allow-list would let it through.
# Prints what ships. Exits 1 (naming each file) on anything else.
param([Parameter(Mandatory)][string]$Release)
$ErrorActionPreference = 'Stop'
$root = (Resolve-Path $Release).Path.TrimEnd([char]'\') + '\'

# exactly what ships
$allow = @(
    '^(redex|redex-world|xr-recomp)\.exe$'
    '^steam_api64\.dll$'
    '^licenses\\[^\\]+\.txt$'
    '^toolchain\\bin\\[^\\]+\.(exe|dll|cfg)$'
    '^toolchain\\(include|x86_64-w64-mingw32|lib\\clang)\\.+'
    '^toolchain\\functions\\[xy]-usa-v0\.2\.0\\(cro\\)?[^\\]+\.(txt|md|csv)$'
)
# never, anywhere: dumps and unpacked game files (RomFS, ExeFS, exheader, CROs), build products (C, objects, the game
# DLL), saves, save states and screenshots of the game (title/top/bottom/card pictures), and the player's own files
# (characters.json and its copies, the dev id, the library, settings, a server's reports and accounts, backups, the
# preview's own folder and a state being made, *.part)
$deny = '\.(cci|3ds|cia|cro|crs|cxi|app|o|obj|state|part|sav|png|bin)$|(^|\\)(xgame\.dll|lookup\.c|(gen|mod)_[^\\]*\.c|build\.ninja)$|(^|\\)(user|romfs|exefs|dumps|build|logs|saves|states|characters|accounts|backups|preview)\\|(^|\\)(characters\.json[^\\]*|redex-id|library\.json[^\\]*|settings\.toml|reports\.jsonl|[^\\]*\.bin\.[^\\]*)$'
# the mingw C runtime's own objects (crt2.o, ...) are part of the toolchain
$crt = '^toolchain\\x86_64-w64-mingw32\\lib\\(?!(gen|mod|lookup)_?)[^\\]+\.o$'
# extensionless files are only the toolchain's C++ headers and its COPYING (an extensionless RomFS file is game data)
$plain = '^toolchain\\(include|lib\\clang\\[^\\]+\\include|x86_64-w64-mingw32\\(include|share))\\'

$files = Get-ChildItem $root -Recurse -File
$bad = foreach ($f in $files) {
    $rel = $f.FullName.Substring($root.Length)
    $why = $null
    if (-not ($allow | Where-Object { $rel -match $_ })) { $why = 'not on the allow-list' }
    elseif ($rel -match $deny -and $rel -notmatch $crt) { $why = 'looks like game data, a save or a build product' }
    elseif ([IO.Path]::GetExtension($rel) -eq '' -and $rel -notmatch $plain) { $why = 'extensionless file outside the toolchain headers' }
    # a test build (--features mp-tests, or sim) has --dev / --dev-id; its --help says MP-TESTS BUILD
    elseif ($rel -match '^(redex|redex-world)\.exe$' -and [Text.Encoding]::ASCII.GetString([IO.File]::ReadAllBytes($f.FullName)).Contains('MP-TESTS BUILD')) { $why = 'built with mp-tests' }
    if ($why) { "$rel ($why)" }
}
if ($bad) { $bad | ForEach-Object { Write-Error "not shipped: $_" -ErrorAction Continue }; exit 1 }

# what ships: the top-level files, then each folder's file count and size
$mb = { param($s) '{0:N1} MB' -f (($s | Measure-Object Length -Sum).Sum / 1MB) }
Get-ChildItem $root -File | ForEach-Object { '{0,-40} {1}' -f $_.Name, (& $mb $_) }
foreach ($d in Get-ChildItem $root -Directory) {
    foreach ($sub in @($d) + @(Get-ChildItem $d.FullName -Directory)) {
        $in = Get-ChildItem $sub.FullName -Recurse -File
        '{0,-40} {1} files, {2}' -f ($sub.FullName.Substring($root.Length) + '\'), @($in).Count, (& $mb $in)
    }
}
Get-ChildItem (Join-Path $root 'licenses') -File -ErrorAction SilentlyContinue | ForEach-Object { "  licenses\$($_.Name)" }
'{0} files, {1} in {2}' -f $files.Count, (& $mb $files), $root
