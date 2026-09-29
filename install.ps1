$ErrorActionPreference = "Stop"
$Repo = if ($env:MUTLINK_REPO) { $env:MUTLINK_REPO } else { "cdcRyan/mutlink" }
$Version = if ($env:MUTLINK_VERSION) { $env:MUTLINK_VERSION } else { "latest" }
$InstallDir = if ($env:MUTLINK_INSTALL_DIR) { $env:MUTLINK_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA "mutlink\bin" }
$UseGh = $env:MUTLINK_USE_GH -eq "1"
if ($Repo -notmatch '^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$') { throw "Invalid repository" }
if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) { throw "Use install.sh on macOS/Linux." }
$Arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
if ($Arch -ne "AMD64") { throw "This release supports Windows x64. Build from source on other architectures." }
if ($UseGh) {
    if (!(Get-Command gh -ErrorAction SilentlyContinue)) { throw "Install GitHub CLI and run gh auth login first." }
    if ($Version -eq "latest") {
        $Version = gh release view --repo $Repo --json tagName --jq .tagName
        if ($LASTEXITCODE -ne 0) { throw "Cannot find release" }
    }
} elseif ($Version -eq "latest") {
    $Version = (Invoke-RestMethod "https://api.github.com/repos/$Repo/releases/latest").tag_name
}
if ($Version -notmatch '^v[0-9]+\.[0-9]+\.[0-9]+([.-][A-Za-z0-9.-]+)?$') { throw "Expected version v0.1.0 or latest" }
$Asset = "mutlink-$Version-x86_64-pc-windows-msvc.zip"
$TempDir = Join-Path ([IO.Path]::GetTempPath()) ([Guid]::NewGuid().ToString())
$Stage = $null
New-Item -ItemType Directory $TempDir | Out-Null
try {
    Write-Output "Downloading $Asset from $Repo..."
    if ($UseGh) {
        gh release download $Version --repo $Repo --pattern $Asset --pattern SHA256SUMS --dir $TempDir
        if ($LASTEXITCODE -ne 0) { throw "Download failed" }
    } else {
        foreach ($File in @($Asset, "SHA256SUMS")) {
            Invoke-WebRequest -UseBasicParsing "https://github.com/$Repo/releases/download/$Version/$File" -OutFile (Join-Path $TempDir $File)
        }
    }
    $Lines = @(Get-Content (Join-Path $TempDir "SHA256SUMS") | Where-Object { $_ -match ("^[a-fA-F0-9]{64}  " + [regex]::Escape($Asset) + "$") })
    if ($Lines.Count -ne 1) { throw "Missing or invalid SHA256 for $Asset" }
    $Expected = ($Lines[0] -split '\s+')[0]
    $Actual = (Get-FileHash (Join-Path $TempDir $Asset) -Algorithm SHA256).Hash
    if ($Expected -ne $Actual) { throw "Checksum mismatch; existing installation was not changed." }
    Expand-Archive (Join-Path $TempDir $Asset) (Join-Path $TempDir "unpacked")
    $Binary = Join-Path $TempDir "unpacked\mutlink.exe"
    & $Binary --version
    if ($LASTEXITCODE -ne 0) { throw "Downloaded binary could not run" }
    New-Item -ItemType Directory -Force $InstallDir | Out-Null
    $Target = Join-Path $InstallDir "mutlink.exe"
    $Stage = Join-Path $InstallDir (".mutlink-" + [Guid]::NewGuid().ToString() + ".exe")
    Copy-Item $Binary $Stage
    if (Test-Path $Target) {
        [IO.File]::Replace($Stage, $Target, "$Target.previous")
    } else {
        [IO.File]::Move($Stage, $Target)
    }
    Write-Output "Installed: $Target"
    Write-Output "Add $InstallDir to your user PATH, then run mutlink --help."
    Write-Output "For upgrades, stop the running mutlink daemon before installing."
} finally {
    Remove-Item $TempDir -Recurse -Force
    if ($Stage -and (Test-Path $Stage)) { Remove-Item $Stage -Force }
}
