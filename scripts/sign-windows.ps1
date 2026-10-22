# Authenticode-sign CretLink's Windows artifacts when a signing identity is
# configured (ST-M006-005).
#
# Signing is OPTIONAL and gated: with no signing material present this script is
# a clean no-op (exit 0) that leaves the binaries unsigned, so a development
# build is always distinguishable from an official signed build. Signing
# material NEVER lives in the repository or a pull-request job — release.yml runs
# only on push to main in the canonical repository, and the secrets below exist
# only on that protected path (REQ-S-019, REQ-S-020).
#
# Inputs (environment, all from protected secrets):
#   CRETLINK_SIGN_PFX_BASE64     base64 of the code-signing PFX (required to sign)
#   CRETLINK_SIGN_PFX_PASSWORD   the PFX password (optional; empty if none)
#   CRETLINK_SIGN_TIMESTAMP_URL  RFC3161 timestamp server (optional; sane default)
#
# On success it signs, with a timestamp, every binary that
# `scripts/gen-artifacts.sh --stage` staged — the archive folder
# target\bundle\cretlink-*\cretlink*.exe and the loose dist\cretlink-*.exe —
# and verifies each signature. It runs between the stage and pack phases, so the
# checksums, archive and SBOMs that `--pack` writes cover the SIGNED bytes
# (ST-M009-004). The decoded PFX is written to a temporary file and removed in a
# finally block whether signing succeeds or fails.

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repoRoot = Split-Path -Parent $PSScriptRoot
$distDir = Join-Path $repoRoot 'dist'
$bundleDir = Join-Path $repoRoot (Join-Path 'target' 'bundle')

$pfxB64 = $env:CRETLINK_SIGN_PFX_BASE64
if ([string]::IsNullOrWhiteSpace($pfxB64)) {
    Write-Host "No signing identity configured (CRETLINK_SIGN_PFX_BASE64 is empty)."
    Write-Host "Artifacts remain UNSIGNED development builds — this is expected off the protected release path."
    exit 0
}

$timestampUrl = $env:CRETLINK_SIGN_TIMESTAMP_URL
if ([string]::IsNullOrWhiteSpace($timestampUrl)) {
    $timestampUrl = 'http://timestamp.digicert.com'
}

$exes = @()
if (Test-Path $distDir) {
    $exes += @(Get-ChildItem -Path $distDir -Filter 'cretlink-*.exe' -File)
}
if (Test-Path $bundleDir) {
    $exes += @(Get-ChildItem -Path $bundleDir -Filter 'cretlink*.exe' -File -Recurse)
}
if ($exes.Count -eq 0) {
    Write-Host "No staged cretlink executables: nothing to sign."
    exit 0
}

$pfxPath = Join-Path ([System.IO.Path]::GetTempPath()) ("cretlink-sign-{0}.pfx" -f ([System.Guid]::NewGuid().ToString('N')))
$cert = $null
try {
    [System.IO.File]::WriteAllBytes($pfxPath, [System.Convert]::FromBase64String($pfxB64))

    $pfxPassword = $env:CRETLINK_SIGN_PFX_PASSWORD
    if ([string]::IsNullOrEmpty($pfxPassword)) {
        $cert = Get-PfxCertificate -FilePath $pfxPath
    } else {
        $securePwd = ConvertTo-SecureString -String $pfxPassword -AsPlainText -Force
        $cert = Get-PfxCertificate -FilePath $pfxPath -Password $securePwd
    }

    Write-Host ("Signing {0} artifact(s) as: {1}" -f $exes.Count, $cert.Subject)
    foreach ($exe in $exes) {
        $result = Set-AuthenticodeSignature -FilePath $exe.FullName -Certificate $cert `
            -HashAlgorithm SHA256 -TimestampServer $timestampUrl
        if ($result.Status -ne 'Valid') {
            throw ("Signing failed for {0}: {1} ({2})" -f $exe.Name, $result.Status, $result.StatusMessage)
        }
        Write-Host ("  signed + verified {0}" -f $exe.Name)
    }
}
finally {
    if (Test-Path $pfxPath) {
        Remove-Item -Path $pfxPath -Force -ErrorAction SilentlyContinue
    }
}
