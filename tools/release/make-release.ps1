# Assembles release/ from nothing: redex.exe, xr-recomp.exe (the build step runs it), redex-world.exe, steam_api64.dll,
# toolchain/ (make-toolchain.ps1) and licenses/ (RE:DEX's and its parts', the toolchain's: LLVM, mingw-w64, ninja,
# ctrtool). Builds the release binaries first unless -NoBuild. Fails unless the folder holds only what
# check-release.ps1 allows, and prints that list.
param(
    [string]$Release = (Join-Path $PSScriptRoot '..\..\release'),
    [string]$Tools = 'D:\Roms-Recomps\PokemonX\tools',
    [switch]$NoBuild
)
$ErrorActionPreference = 'Stop'
$repo = Resolve-Path (Join-Path $PSScriptRoot '..\..')
$Release = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Release)
if ($Release -eq [IO.Path]::GetPathRoot($Release) -or (Test-Path (Join-Path $Release '.git'))) { throw "$Release is not a release folder" }
if (-not $NoBuild) {
    Push-Location $repo
    $ErrorActionPreference = 'Continue'   # cargo writes progress to stderr
    cargo build --release -p xr-runtime -p redex-world -p xr-recomp
    $ErrorActionPreference = 'Stop'
    if ($LASTEXITCODE) { Pop-Location; throw 'cargo build failed' }
    Pop-Location
}
# nothing left over from an earlier run (a test build, a save, a picture) can ship
if (Test-Path $Release) { Remove-Item $Release -Recurse -Force }
& (Join-Path $PSScriptRoot 'make-toolchain.ps1') -Tools $Tools -Release $Release
if ($LASTEXITCODE) { throw 'make-toolchain failed' }
foreach ($f in 'redex.exe', 'xr-recomp.exe', 'redex-world.exe', 'steam_api64.dll') { Copy-Item (Join-Path $repo "target\release\$f") $Release }

$lic = New-Item -ItemType Directory (Join-Path $Release 'licenses')
$txt = { param($n) if ($n -like '*.txt' -or $n -like '*.TXT') { $n } else { "$n.txt" } }
Push-Location $repo
foreach ($p in git ls-files -- 'LICENSE' '*/LICENSE*' '*/NOTICE*' '*/COPYING*') {
    # LICENSE -> RE-DEX-LICENSE.txt, crates/xr-hle/NOTICE -> xr-hle-NOTICE.txt, third_party/muda/LICENSE-MIT -> muda-LICENSE-MIT.txt
    $name = if ($p -eq 'LICENSE') { 'RE-DEX-LICENSE' } else { ($p -replace '^(crates|third_party)/', '') -replace '/', '-' }
    Copy-Item $p (Join-Path $lic (& $txt $name))
}
Pop-Location
Copy-Item (Join-Path $Tools 'llvm-mingw\LICENSE.TXT') (Join-Path $lic 'llvm-mingw-LICENSE.txt')
Get-ChildItem (Join-Path $Tools 'llvm-mingw\x86_64-w64-mingw32\share\mingw32') -File | ForEach-Object { Copy-Item $_.FullName (Join-Path $lic (& $txt "mingw-w64-$($_.Name)")) }
Copy-Item (Join-Path $PSScriptRoot 'licenses\*.txt') $lic

# the guard itself first: it must refuse saves, states, card pictures, characters and account files
& (Join-Path $PSScriptRoot 'test-check-release.ps1')
if ($LASTEXITCODE) { throw 'check-release.ps1 lets player files through' }
& (Join-Path $PSScriptRoot 'check-release.ps1') -Release $Release
if ($LASTEXITCODE) { exit 1 }
