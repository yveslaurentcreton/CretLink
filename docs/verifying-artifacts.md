# Verifying CretLink release artifacts

Every CretLink GitHub release attaches:

- one archive per operating system — `cretlink-<version>-windows-x86_64.zip`,
  `cretlink-<version>-macos-aarch64.tar.gz` and
  `cretlink-<version>-linux-x86_64.tar.gz`. Each holds one folder with the
  application `cretlink` (Give help, and on Windows Get help, the program the
  customer runs) and its console build `cretlink-cli`, plus `README.txt`,
  `LICENSE` and the target's `CHECKSUMS-<target>.txt`;
- `SHA256SUMS.txt` — SHA-256 over the archives;
- `cretlink-relay-server-<target>[.exe]` — the relay server, operator-hosted
  infrastructure on neither endpoint, so not in the archives;
- `CHECKSUMS-<target>.txt` — SHA-256 over every program built for that target;
- `dependency-inventory-<target>.txt` — the resolved, pinned dependency graph the
  programs were built from;
- `sbom-<binary>-<target>.cdx.json` — one CycloneDX SBOM per shipped program
  (`cretlink`, `cretlink-cli` and the relay server), carrying the release
  version.

Every program reports its release version: `cretlink version` or
`cretlink-cli version`.

This page tells you how to confirm that the files you downloaded are the ones the
release published. Do the checksum check on every platform; do the signature
check on Windows when the release is signed. No CretLink release has been signed
so far, so the checksums are currently the only integrity check.

## 1. Verify the SHA-256 checksums

First check the archive you downloaded against `SHA256SUMS.txt`, in the folder
holding both:

```sh
sha256sum -c --ignore-missing SHA256SUMS.txt          # Linux
shasum -a 256 -c --ignore-missing SHA256SUMS.txt      # macOS
```

```powershell
# Windows
$line = Select-String -Path .\SHA256SUMS.txt -Pattern 'windows-x86_64.zip'
$expected, $name = $line.Line -split '\s+', 2
if ((Get-FileHash -Algorithm SHA256 $name).Hash.ToLowerInvariant() -eq $expected) { "OK    $name" } else { "FAIL  $name" }
```

Then unpack it and check the programs inside against the `CHECKSUMS` file that
came with them. The checksum file covers exactly the bytes each program shipped
with — for a signed Windows release, that is the *signed* program, because
signing runs before the checksums are written. It also lists the relay server,
which is not in the archive; ignore that line unless you downloaded it.

### Windows (PowerShell)

```powershell
# From the unpacked folder:
Get-Content .\CHECKSUMS-x86_64-pc-windows-msvc.txt | ForEach-Object {
    $expected, $name = $_ -split '\s+', 2
    if (-not (Test-Path $name)) { return }
    $actual = (Get-FileHash -Algorithm SHA256 -Path $name).Hash.ToLowerInvariant()
    if ($actual -eq $expected) { "OK    $name" } else { "FAIL  $name" }
}
```

### Linux

```sh
sha256sum -c --ignore-missing CHECKSUMS-x86_64-unknown-linux-gnu.txt
```

### macOS

```sh
shasum -a 256 -c --ignore-missing CHECKSUMS-aarch64-apple-darwin.txt
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

Check both programs of the archive, `cretlink.exe` (the one the customer runs)
and `cretlink-cli.exe`, from the unpacked folder:

```powershell
Get-AuthenticodeSignature .\cretlink.exe, .\cretlink-cli.exe |
    Format-List Path, Status, StatusMessage, SignerCertificate
```

- `Status : Valid` with a trusted, timestamped signer means the binary is an
  official signed build.
- `Status : NotSigned` means an unsigned build — expected for every release so
  far (see above); the checksums are then the assurance.

Or with the Windows SDK `signtool`:

```powershell
signtool verify /pa /v .\cretlink.exe .\cretlink-cli.exe
```

A customer who received only `cretlink.exe` (or a copy renamed to
`CretLink-Support.exe`) checks that one file the same way.

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

## 4. Verify the relay server image

The container image of the relay server is not a release file: it lives in
the registry, as `ghcr.io/yveslaurentcreton/cretlink-relay-server:<version>`.
Every published version carries a signed provenance attestation that names
the repository, the workflow and the commit it was built from. Verify it with
the GitHub CLI before you run a version:

```sh
gh attestation verify oci://ghcr.io/yveslaurentcreton/cretlink-relay-server:<version> \
  --repo yveslaurentcreton/CretLink
```

The command fails when the image was not built by this repository's release
workflow. Its output names the commit; compare it with the tag `v<version>`.
To see the bill of materials of an architecture, add
`--predicate-type https://cyclonedx.org/bom`.

Run the image by its digest when you want to be sure that what you verified
is what runs: `docker pull` prints the digest, and
`ghcr.io/yveslaurentcreton/cretlink-relay-server@sha256:<digest>` names
exactly that image.
