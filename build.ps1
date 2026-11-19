# Builds Ferrocap and assembles dist\ (Ferrocap.exe + ffmpeg.exe).
# Installs anything missing first (Rust GNU toolchain, WinLibs MinGW, FFmpeg) via winget,
# so on a fresh PC you only need to run this file (or double-click build.cmd).
# Usage:  .\build.ps1            (release)
#         .\build.ps1 -Debug
param([switch]$Debug)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$pk = Join-Path $env:LOCALAPPDATA 'Microsoft\WinGet\Packages'

function Update-Path {
    # pick up PATH changes made by installers without needing a new terminal
    $machine = [Environment]::GetEnvironmentVariable('Path', 'Machine')
    $user = [Environment]::GetEnvironmentVariable('Path', 'User')
    $env:Path = "$env:USERPROFILE\.cargo\bin;$machine;$user"
}

function Install-Package([string]$id, [string]$what) {
    if (-not (Get-Command winget -ErrorAction SilentlyContinue)) {
        throw "winget isn't available, so $what can't be installed automatically. Install 'App Installer' from the Microsoft Store, then run this again."
    }
    Write-Host "Installing $what ($id)..." -ForegroundColor Cyan
    winget install --id $id --exact --silent --accept-package-agreements --accept-source-agreements --disable-interactivity
    # winget returns non-zero for "already installed"; the checks below decide whether it worked
    Update-Path
}

function Find-MinGW {
    Get-ChildItem $pk -Directory -Filter 'BrechtSanders.WinLibs*' -ErrorAction SilentlyContinue |
        ForEach-Object { Join-Path $_.FullName 'mingw64\bin' } |
        Where-Object { Test-Path (Join-Path $_ 'gcc.exe') } | Select-Object -First 1
}

function Find-FFmpeg {
    $ff = Get-ChildItem $pk -Directory -Filter 'Gyan.FFmpeg*' -ErrorAction SilentlyContinue |
        ForEach-Object { Get-ChildItem $_.FullName -Recurse -Filter ffmpeg.exe -ErrorAction SilentlyContinue } |
        Select-Object -First 1
    if ($ff) { return $ff.FullName }
    $cmd = Get-Command ffmpeg.exe -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
}

Update-Path

# 1. Rust (rustup + the GNU toolchain Ferrocap builds with)
if (-not (Get-Command rustup -ErrorAction SilentlyContinue)) {
    Install-Package 'Rustlang.Rustup' 'Rust'
    if (-not (Get-Command rustup -ErrorAction SilentlyContinue)) { throw 'Rust install failed. Install it from https://rustup.rs and run this again.' }
}
$toolchain = 'stable-x86_64-pc-windows-gnu'
if (-not ((rustup toolchain list) -match [regex]::Escape($toolchain))) {
    Write-Host "Installing Rust toolchain $toolchain..." -ForegroundColor Cyan
    rustup toolchain install $toolchain --profile minimal
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
}
$env:RUSTUP_TOOLCHAIN = $toolchain  # use it for this build without changing your default

# 2. MinGW (linker for the GNU toolchain)
$mingw = Find-MinGW
if (-not $mingw) {
    Install-Package 'BrechtSanders.WinLibs.POSIX.UCRT' 'WinLibs MinGW'
    $mingw = Find-MinGW
    if (-not $mingw) { throw 'WinLibs MinGW install failed. Run: winget install BrechtSanders.WinLibs.POSIX.UCRT' }
}
$env:Path = "$mingw;$env:Path"

# 3. FFmpeg (copied next to Ferrocap.exe for local testing)
$dist = Join-Path $PSScriptRoot 'dist'
if (-not (Test-Path (Join-Path $dist 'ffmpeg.exe')) -and -not (Find-FFmpeg)) {
    Install-Package 'Gyan.FFmpeg' 'FFmpeg'
}

# short target dir: some build scripts break on long Windows paths
if (-not $env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR = Join-Path $env:LOCALAPPDATA 'Temp\fcbuild' }

# keep your Windows user folder out of the exe (Rust embeds dependency paths for error messages)
$env:RUSTFLAGS = "--remap-path-prefix=$($env:USERPROFILE)=~"

Push-Location $PSScriptRoot
try {
    $profileName = if ($Debug) { 'debug' } else { 'release' }
    if ($Debug) { cargo build } else { cargo build --release }
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
} finally { Pop-Location }

New-Item -ItemType Directory -Force $dist | Out-Null
Copy-Item (Join-Path $env:CARGO_TARGET_DIR "$profileName\ferrocap.exe") (Join-Path $dist 'Ferrocap.exe') -Force

if (-not (Test-Path (Join-Path $dist 'ffmpeg.exe'))) {
    $ff = Find-FFmpeg
    if ($ff) { Copy-Item $ff (Join-Path $dist 'ffmpeg.exe') }
    else { Write-Warning 'ffmpeg.exe not found. Put one next to Ferrocap.exe.' }
}
Write-Host "Built $dist\Ferrocap.exe" -ForegroundColor Green
