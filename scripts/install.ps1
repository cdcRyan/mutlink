$ErrorActionPreference = "Stop"
$ProjectDir = Split-Path $PSScriptRoot -Parent
$InstallDir = if ($env:MUTLINK_INSTALL_DIR) { $env:MUTLINK_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA "mutlink\bin" }
$Binary = Join-Path $ProjectDir "target\release\mutlink.exe"
if (Test-Path (Join-Path $ProjectDir "mutlink.exe")) {
    $Binary = Join-Path $ProjectDir "mutlink.exe"
}
if (!(Test-Path $Binary)) {
    if (!(Get-Command cargo -ErrorAction SilentlyContinue)) {
        throw "Install Rust 1.88+ from https://rustup.rs and the MSVC build tools, or use a prebuilt mutlink.exe."
    }
    cargo build --manifest-path (Join-Path $ProjectDir "Cargo.toml") --release --locked
    if ($LASTEXITCODE -ne 0) { throw "Build failed" }
}
New-Item -ItemType Directory -Force $InstallDir | Out-Null
$Target = Join-Path $InstallDir "mutlink.exe"
if (Test-Path $Target) { Copy-Item $Target "$Target.previous" -Force }
Copy-Item $Binary $Target -Force
Write-Output "Installed: $Target"
Write-Output "Add $InstallDir to your user PATH, then run mutlink."
