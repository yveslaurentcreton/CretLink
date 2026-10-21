# Verifying CretLink release artifacts

Every CretLink GitHub release attaches, per platform target triple:

- the five binaries — `cretlink-console`, `cretlink-mcp`, `cretlink-shell`
  (technician computer), `cretlink-support-agent` (the only binary placed on
  the customer computer), and `cretlink-relay-server` (operator-hosted public
  infrastructure, on neither endpoint);
- `CHECKSUMS-<target>.txt` — SHA-256 over every binary for that target;
- `dependency-inventory-<target>.txt` — the resolved, pinned dependency graph the
  binaries were built from;
- `sbom-<binary>-<target>.cdx.json` — one CycloneDX SBOM per shipped binary,
  when available.

This page tells you how to confirm that the files you downloaded are the ones the
release published. Do the checksum check on every platform; do the signature
check on Windows when the release is signed. No CretLink release has been signed
so far, so the checksums are currently the only integrity check.

## 1. Verify the SHA-256 checksums

The checksum file covers exactly the bytes each binary shipped with — for a
signed Windows release, that is the *signed* binary, because signing runs before
the checksums are written.

### Windows (PowerShell)

```powershell
# From the folder holding the downloaded binaries and CHECKSUMS file:
Get-Content .\CHECKSUMS-x86_64-pc-windows-msvc.txt | ForEach-Object {
    $expected, $name = $_ -split '\s+', 2
    $actual = (Get-FileHash -Algorithm SHA256 -Path $name).Hash.ToLowerInvariant()
    if ($actual -eq $expected) { "OK    $name" } else { "FAIL  $name" }
}
```

### Linux

```sh
sha256sum -c CHECKSUMS-x86_64-unknown-linux-gnu.txt
```

### macOS

```sh
shasum -a 256 -c CHECKSUMS-aarch64-apple-darwin.txt
```

Every line must report `OK`. A `FAIL` means the file differs from what the
release published — do not run it.

## 2. Verify the Authenticode signature (Windows)

Official Windows builds are Authenticode-signed **when a signing identity is
available to the release**. Signing is implemented but gated: if no signing
identity is configured for a given release, the Windows binaries are unsigned.
No signing identity has been configured so far, so every release to date is
unsigned and an unsigned binary is expected — treat the signature as an
additional assurance once it exists, and always rely on the checksums above as
the baseline.

### Is a binary signed, and by whom?

```powershell
Get-AuthenticodeSignature .\cretlink-support-agent-x86_64-pc-windows-msvc.exe |
    Format-List Status, StatusMessage, SignerCertificate
```

- `Status : Valid` with a trusted, timestamped signer means the binary is an
  official signed build.
- `Status : NotSigned` means an unsigned build — expected for every release so
  far (see above); the checksums are then the assurance.

Or with the Windows SDK `signtool`:

```powershell
signtool verify /pa /v .\cretlink-support-agent-x86_64-pc-windows-msvc.exe
```

### Expected signer identity

When CretLink publishes signed builds, the expected signer's subject/thumbprint
is recorded here and in the signed release's notes. No signing identity has
been provisioned yet, so there is no expected signer and Windows binaries are
unsigned.

## 3. Cross-check provenance (optional)

`dependency-inventory-<target>.txt` and the `sbom-<binary>-<target>.cdx.json`
files list the exact pinned dependencies the release was built from, so you can
compare against the locked workspace at the release tag. They describe
provenance; they are not a substitute for the checksum and signature checks
above.
