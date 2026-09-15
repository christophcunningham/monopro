param(
    [switch]$Unsigned
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

if ($env:OS -ne "Windows_NT") {
    throw "The Windows package must be built on Windows."
}

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$Manifest = Join-Path $RepoRoot "Cargo.toml"
$VersionMatch = Select-String -Path $Manifest -Pattern '^version = "([^"]+)"' | Select-Object -First 1
if ($null -eq $VersionMatch) {
    throw "Could not read workspace version from Cargo.toml."
}
$Version = $VersionMatch.Matches[0].Groups[1].Value

if ($null -eq (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw "cargo was not found on PATH."
}
$Iscc = Get-Command iscc -ErrorAction SilentlyContinue
if ($null -eq $Iscc) {
    throw "Inno Setup's iscc compiler was not found on PATH."
}
if (-not $Unsigned) {
    if ([string]::IsNullOrWhiteSpace($env:MONOPRO_CERT_SHA1)) {
        throw "MONOPRO_CERT_SHA1 is required. Use -Unsigned only for local package validation."
    }
    if ([string]::IsNullOrWhiteSpace($env:MONOPRO_TIMESTAMP_URL)) {
        throw "MONOPRO_TIMESTAMP_URL is required. Use -Unsigned only for local package validation."
    }
    if ($null -eq (Get-Command signtool.exe -ErrorAction SilentlyContinue)) {
        throw "signtool.exe was not found on PATH."
    }
}

$Dist = Join-Path $RepoRoot "dist"
New-Item -ItemType Directory -Force -Path $Dist | Out-Null

Push-Location $RepoRoot
try {
    cargo build --release --locked --target x86_64-pc-windows-msvc
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build failed with exit code $LASTEXITCODE."
    }

    $CompilerArgs = @(
        "--define=AppVersion=$Version",
        "--define=RepoRoot=$RepoRoot",
        "--output-dir=$Dist"
    )

    if (-not $Unsigned) {
        $Signer = (Resolve-Path (Join-Path $PSScriptRoot "sign.cmd")).Path
        $CompilerArgs += "--define=SignedBuild=1"
        $CompilerArgs += ('--signtool=monopro=$q' + $Signer + '$q $f')
    }

    & $Iscc.Path @CompilerArgs (Join-Path $PSScriptRoot "monopro.iss")
    if ($LASTEXITCODE -ne 0) {
        throw "Inno Setup failed with exit code $LASTEXITCODE."
    }

    $Installer = Join-Path $Dist "monopro-$Version-windows-x86_64-setup.exe"
    if (-not (Test-Path $Installer)) {
        throw "The expected installer was not produced: $Installer"
    }

    if (-not $Unsigned) {
        & signtool.exe verify /pa /all $Installer
        if ($LASTEXITCODE -ne 0) {
            throw "Authenticode verification failed with exit code $LASTEXITCODE."
        }
        & (Join-Path $PSScriptRoot "verify.ps1") -Installer $Installer -RequireSignature
    }
    else {
        & (Join-Path $PSScriptRoot "verify.ps1") -Installer $Installer
    }

    Write-Host $Installer
}
finally {
    Pop-Location
}
