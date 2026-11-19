# Builds Ferrocap and assembles dist\ (Ferrocap.exe + ffmpeg.exe).
# Usage:  .\build.ps1            (release)
#         .\build.ps1 -Debug
param([switch]$Debug)
$ErrorActionPreference = 'Stop'

$pk = Join-Path $env:LOCALAPPDATA 'Microsoft\WinGet\Packages'
$mingw = Get-ChildItem $pk -Directory -Filter 'BrechtSanders.WinLibs*' -ErrorAction SilentlyContinue |
    ForEach-Object { Join-Path $_.FullName 'mingw64\bin' } | Where-Object { Test-Path $_ } | Select-Object -First 1
if ($mingw) { $env:Path = "$mingw;$env:Path" }
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
# short target dir: some build scripts break on long Windows paths
if (-not $env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR = Join-Path $env:LOCALAPPDATA 'Temp\fcbuild' }

# keep your Windows user folder out of the exe (Rust embeds dependency paths for error messages)
$env:RUSTFLAGS = "--remap-path-prefix=$($env:USERPROFILE)=~"

$profileName = if ($Debug) { 'debug' } else { 'release' }
if ($Debug) { cargo build } else { cargo build --release }
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

$dist = Join-Path $PSScriptRoot 'dist'
New-Item -ItemType Directory -Force $dist | Out-Null
Copy-Item (Join-Path $env:CARGO_TARGET_DIR "$profileName\ferrocap.exe") (Join-Path $dist 'Ferrocap.exe') -Force

if (-not (Test-Path (Join-Path $dist 'ffmpeg.exe'))) {
    $ff = Get-ChildItem $pk -Directory -Filter 'Gyan.FFmpeg*' -ErrorAction SilentlyContinue |
        ForEach-Object { Get-ChildItem $_.FullName -Recurse -Filter ffmpeg.exe } | Select-Object -First 1
    if ($ff) { Copy-Item $ff.FullName (Join-Path $dist 'ffmpeg.exe') }
    else { Write-Warning 'ffmpeg.exe not found. Put one next to Ferrocap.exe.' }
}
Write-Host "Built $dist\Ferrocap.exe"
