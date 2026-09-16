# Compile the real installer definition with a disposable payload before building Rust.
$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$Definition = Join-Path $PSScriptRoot "monopro.iss"
$VersionMatch = Select-String -Path (Join-Path $RepoRoot "Cargo.toml") -Pattern '^version = "([^"]+)"' | Select-Object -First 1
$Version = $VersionMatch.Matches[0].Groups[1].Value
$TestRoot = Join-Path ([System.IO.Path]::GetTempPath()) ([System.Guid]::NewGuid().ToString())
try {
    New-Item -ItemType Directory -Path $TestRoot | Out-Null
    foreach ($Line in Get-Content $Definition) {
        if ($Line -match '^Source: "([^"]+)";' -and -not $Matches[1].StartsWith('target\')) {
            $Source = $Matches[1]
            $Destination = Join-Path $TestRoot $Source
            New-Item -ItemType Directory -Force -Path (Split-Path $Destination) | Out-Null
            Copy-Item (Join-Path $RepoRoot $Source) $Destination
        }
    }
    $Payload = Join-Path $TestRoot 'target\x86_64-pc-windows-msvc\release\monopro.exe'
    New-Item -ItemType Directory -Force -Path (Split-Path $Payload) | Out-Null
    Copy-Item (Join-Path $env:WINDIR 'System32\where.exe') $Payload
    & iscc "/DAppVersion=$Version" "/DRepoRoot=$TestRoot" "/O$TestRoot\dist" $Definition
    if ($LASTEXITCODE -ne 0) { throw "Installer definition failed to compile." }
    $Installer = Join-Path $TestRoot "dist\monopro-$Version-windows-x86_64-setup.exe"
    $Info = [System.Diagnostics.FileVersionInfo]::GetVersionInfo($Installer)
    $Info | Select-Object ProductName, ProductVersion, FileVersion | ConvertTo-Json
    if ($Info.ProductName -ne 'monopro' -or $Info.ProductVersion -notlike "$Version*") {
        throw "Installer definition emitted unexpected version metadata."
    }
}
finally {
    if (Test-Path $TestRoot) { Remove-Item -Recurse -Force $TestRoot }
}
