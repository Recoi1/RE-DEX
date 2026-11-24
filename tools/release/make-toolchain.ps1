# Assembles release/toolchain/ (the trimmed clang the game-build step ships) from the dev toolchain.
#   bin/        clang (+ its .cfg files), lld, ninja, ctrtool + the DLLs they load
#   include/, lib/clang/<v>/{include,lib/windows/builtins}, x86_64-w64-mingw32/   mingw headers + libs (llvm-mingw layout)
#   functions/  the committed function tables
# Prints the total size; fails if anything under release/ is not on check-release.ps1's allow-list.
param(
    [string]$Tools = 'D:\Roms-Recomps\PokemonX\tools',
    [string]$Release = (Join-Path $PSScriptRoot '..\..\release'),
    [int]$MaxMB = 400
)
$ErrorActionPreference = 'Stop'
$repo = Resolve-Path (Join-Path $PSScriptRoot '..\..')
$Release = [IO.Path]::GetFullPath($Release)
$tc = Join-Path $Release 'toolchain'
$mingw = Join-Path $Tools 'llvm-mingw'
if (Test-Path $tc) { Remove-Item $tc -Recurse -Force }
New-Item -ItemType Directory -Force "$tc\bin" | Out-Null

# clang.exe is a small wrapper that runs clang-23.exe; lld is ld.lld.exe
$exes = 'clang.exe', 'clang-23.exe', 'ld.lld.exe', 'x86_64-w64-mingw32-clang.exe'
$dlls = 'libclang-cpp.dll', 'libLLVM-23.dll', 'libc++.dll', 'libunwind.dll', 'libwinpthread-1.dll', 'libffi-8.dll'
foreach ($f in $exes + $dlls) { Copy-Item (Join-Path $mingw "bin\$f") "$tc\bin" }
# the .cfg files pick compiler-rt, libunwind, libc++ and -fuse-ld=lld (without them clang looks for libgcc and a shell-script ld)
Copy-Item (Join-Path $mingw 'bin\x86_64-*.cfg'), (Join-Path $mingw 'bin\mingw32-common.cfg') "$tc\bin"
Copy-Item (Join-Path $Tools 'ninja\ninja.exe') "$tc\bin"
Copy-Item (Join-Path $Tools 'ctrtool\*') "$tc\bin" -Recurse   # ctrtool.exe (+ any DLL beside it)

Copy-Item (Join-Path $mingw 'x86_64-w64-mingw32') "$tc\x86_64-w64-mingw32" -Recurse
Copy-Item (Join-Path $mingw 'include') "$tc\include" -Recurse
$ver = (Get-ChildItem (Join-Path $mingw 'lib\clang') -Directory | Select-Object -First 1).Name
$rt = "$tc\lib\clang\$ver"
New-Item -ItemType Directory -Force "$rt\lib\windows" | Out-Null
Copy-Item (Join-Path $mingw "lib\clang\$ver\include") "$rt\include" -Recurse
Copy-Item (Join-Path $mingw "lib\clang\$ver\lib\windows\libclang_rt.builtins-x86_64.a") "$rt\lib\windows"

New-Item -ItemType Directory -Force "$tc\functions" | Out-Null
foreach ($d in 'x-usa-v0.2.0', 'y-usa-v0.2.0') { Copy-Item (Join-Path $repo "functions\$d") "$tc\functions\$d" -Recurse }

# only what ships, never game data
& (Join-Path $PSScriptRoot 'check-release.ps1') -Release $Release | Out-Null
if ($LASTEXITCODE) { exit 1 }

$mb = (Get-ChildItem $tc -Recurse -File | Measure-Object Length -Sum).Sum / 1MB
'{0:N0} MB in {1}' -f $mb, $tc
if ($mb -gt $MaxMB) { Write-Error "toolchain is over $MaxMB MB"; exit 1 }


