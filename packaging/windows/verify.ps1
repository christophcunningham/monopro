param(
    [string]$Installer,
    [switch]$RequireSignature
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$VersionMatch = Select-String -Path (Join-Path $RepoRoot "Cargo.toml") -Pattern '^version = "([^"]+)"' | Select-Object -First 1
if ($null -eq $VersionMatch) {
    throw "Could not read workspace version from Cargo.toml."
}
$Version = $VersionMatch.Matches[0].Groups[1].Value
if ([string]::IsNullOrWhiteSpace($Installer)) {
    $Installer = Join-Path $RepoRoot "dist\monopro-$Version-windows-x86_64-setup.exe"
}
$Installer = (Resolve-Path $Installer).Path
$Binary = (Resolve-Path (Join-Path $RepoRoot "target\x86_64-pc-windows-msvc\release\monopro.exe")).Path

function Get-PeMachine([string]$Path) {
    $Stream = [System.IO.File]::OpenRead($Path)
    $Reader = [System.IO.BinaryReader]::new($Stream)
    try {
        if ($Reader.ReadUInt16() -ne 0x5A4D) {
            throw "$Path is not a PE executable."
        }
        $Stream.Position = 0x3C
        $PeOffset = $Reader.ReadInt32()
        $Stream.Position = $PeOffset
        if ($Reader.ReadUInt32() -ne 0x00004550) {
            throw "$Path has no PE signature."
        }
        return $Reader.ReadUInt16()
    }
    finally {
        $Reader.Dispose()
    }
}

if ((Get-PeMachine $Binary) -ne 0x8664) {
    throw "The packaged monopro executable is not x86-64."
}

# Compiler-generated panic locations must not disclose the build machine's paths.
$BinaryBytes = [System.IO.File]::ReadAllBytes($Binary)
foreach ($Encoding in @([System.Text.Encoding]::UTF8, [System.Text.Encoding]::Unicode)) {
    $BinaryText = $Encoding.GetString($BinaryBytes)
    foreach ($PrivateRoot in @($RepoRoot, $env:USERPROFILE)) {
        if (-not [string]::IsNullOrWhiteSpace($PrivateRoot)) {
            foreach ($Variant in @($PrivateRoot, $PrivateRoot.Replace('\', '/'))) {
                if ($BinaryText.Contains($Variant)) {
                    throw "The executable contains an unremapped build-machine path."
                }
            }
        }
    }
}
if ([System.IO.Path]::GetFileName($Installer) -ne "monopro-$Version-windows-x86_64-setup.exe") {
    throw "The installer name does not match the workspace version and architecture."
}
$VersionInfo = [System.Diagnostics.FileVersionInfo]::GetVersionInfo($Installer)
if ($VersionInfo.ProductName -ne "monopro" -or $VersionInfo.ProductVersion -notlike "$Version*") {
    throw "Expected monopro $Version; installer reports '$($VersionInfo.ProductName)' '$($VersionInfo.ProductVersion)'."
}

$Signature = Get-AuthenticodeSignature $Installer
if ($RequireSignature -and $Signature.Status -ne [System.Management.Automation.SignatureStatus]::Valid) {
    throw "The release installer does not have a valid Authenticode signature: $($Signature.Status)."
}
if (-not $RequireSignature -and $Signature.Status -ne [System.Management.Automation.SignatureStatus]::NotSigned) {
    throw "An unsigned candidate unexpectedly reports signature status $($Signature.Status)."
}

$Hash = (Get-FileHash -Algorithm SHA256 $Installer).Hash.ToLowerInvariant()
$Checksum = "$Installer.sha256"
$Line = "$Hash  $([System.IO.Path]::GetFileName($Installer))"
Set-Content -Path $Checksum -Value $Line -Encoding ascii
Write-Host $Installer
Write-Host $Checksum
