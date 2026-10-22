#!/usr/bin/env bash
# Generate verifiable release artifacts for CretLink (ST-M006-004, ST-M006-014,
# ST-M009-004).
#
# Portable across the release matrix (windows / macos / ubuntu runners, all of
# which provide a bash shell) and runnable locally in any bash (Linux, macOS, or
# Git Bash on Windows), so a maintainer — or CI — produces the same artifacts
# from the same commit.
#
# Usage: scripts/gen-artifacts.sh [--stage | --pack] [VERSION]
#
# Without an option it does both phases. The release workflow runs `--stage`
# (build and stage the binaries), then signs the staged Windows binaries in a
# separate step that alone holds the signing secrets (scripts/sign-windows.ps1),
# then runs `--pack` (checksums, archive, inventory and SBOMs over the signed
# bytes).
#
# VERSION (argument, or the VERSION or CRETLINK_VERSION environment variable)
# is the release version the workflow computed with scripts/next-version.mjs;
# it defaults to 0.0.0-dev. It is stamped into the binaries at compile time
# (CRETLINK_VERSION, read by cretlink_shared::RELEASE_VERSION) and names the
# archive. This script never chooses a version itself (REQ-N-018).
#
# Under dist/ it writes, for the host target triple:
#   - cretlink-<version>-<os>-<arch>.zip (Windows) or .tar.gz (macOS, Linux):
#     one folder with the application `cretlink`, the console build
#     `cretlink-cli` and the other technician binaries — and on Windows the
#     headless support agent — plus README.txt, LICENSE and the checksum file
#     (REQ-N-021);
#   - cretlink-relay-server-<target>[.exe]: the relay server, which runs on
#     neither endpoint and so is not in the technician archive;
#   - CHECKSUMS-<target>.txt   SHA-256 over every binary for this target;
#   - dependency-inventory-<target>.txt   the exact normal and build dependency
#     graph and pinned versions of the shipped packages (cargo tree, locked);
#   - sbom-<binary>-<target>.cdx.json   a CycloneDX SBOM per shipped binary,
#     when cargo-cyclonedx is installed, carrying the version. The release
#     workflow installs it and sets CRETLINK_REQUIRE_SBOM=1, so official
#     artifacts fail rather than ship without SBOMs; locally a missing tool is
#     reported, never faked.
# The release job adds SHA256SUMS.txt over the archives of every target.
set -euo pipefail

phase="all"
case "${1:-}" in
  --stage) phase="stage"; shift ;;
  --pack) phase="pack"; shift ;;
esac
version="${1:-${VERSION:-${CRETLINK_VERSION:-0.0.0-dev}}}"
if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]; then
  echo "error: '$version' is not a semantic version" >&2
  exit 1
fi
export CRETLINK_VERSION="$version"
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

# Host target triple, straight from the compiler that will build the binaries.
target="$(rustc -vV | sed -n 's/^host: //p')"
arch="${target%%-*}"
case "$target" in
  *-windows-*) os="windows"; exe=".exe" ;;
  *-apple-darwin) os="macos"; exe="" ;;
  *-linux-*) os="linux"; exe="" ;;
  *) echo "error: unsupported target $target" >&2; exit 1 ;;
esac

out="$repo_root/dist"
mkdir -p "$out"
bundle_name="cretlink-$version-$os-$arch"
stage="$repo_root/target/bundle/$bundle_name"

# The shippable binaries and their owning packages, as
# <binary>:<package>:<package directory>:<placement>. Placement `bundle` goes
# into the technician archive; `loose` is attached on its own. The `cretlink-cli`
# command, the console, the MCP bridge and the host shell run on the technician
# computer; the support agent is the only binary placed on the customer computer
# and ships in the Windows archive only, because it is Windows-only; the relay
# server runs on neither endpoint — it is operator-hosted infrastructure that
# forwards opaque end-to-end Noise sessions and holds no session key
# (ST-M007-007).
bins=(
  "cretlink-cli:cretlink-cli:apps/cli:bundle"
  "cretlink-console:cretlink-console:apps/console:bundle"
  "cretlink-mcp:cretlink-mcp-bridge:apps/mcp-bridge:bundle"
  "cretlink-shell:cretlink-shell:apps/shell-cli:bundle"
  "cretlink-relay-server:cretlink-relay-server:apps/relay-server:loose"
)
if [[ "$os" == windows ]]; then
  bins+=("cretlink-support-agent:cretlink-support-agent:apps/support-agent:bundle")
fi

# The application `cretlink` (ST-M009-005, ADR-0018): both roles and the
# subcommands in one program, built from the detached Tauri crate with its
# frontend embedded. It is the program the customer runs on Windows (Get help).
app_dir="apps/app"
app_entry="cretlink:cretlink-app:$app_dir/src-tauri:bundle"
staged_bins=("$app_entry" "${bins[@]}")

field() { # field <entry> <index 1..4>
  echo "$1" | cut -d: -f"$2"
}

build_args=()
for entry in "${bins[@]}"; do
  build_args+=("-p" "$(field "$entry" 2)")
done

# Where a staged binary lives: in the archive folder, or loose in dist/.
staged_path() { # staged_path <entry>
  local bin
  bin="$(field "$1" 1)"
  if [[ "$(field "$1" 4)" == bundle ]]; then
    echo "$stage/$bin$exe"
  else
    echo "$out/$bin-$target$exe"
  fi
}

if [[ "$phase" != pack ]]; then
  echo "== building release binaries for $target (version $version) =="
  cargo build --release --locked "${build_args[@]}"
  echo "== building the application (frontend, then the Tauri crate) =="
  (cd "$app_dir" && npm ci --ignore-scripts && npm run build)
  cargo build --release --locked --manifest-path "$app_dir/src-tauri/Cargo.toml" \
    --features tauri/custom-protocol
  rm -rf "$stage"
  mkdir -p "$stage"
  echo "== staging binaries =="
  for entry in "${staged_bins[@]}"; do
    if [[ "$entry" == "$app_entry" ]]; then
      src="$repo_root/$app_dir/src-tauri/target/release/cretlink$exe"
    else
      src="$repo_root/target/release/$(field "$entry" 1)$exe"
    fi
    if [[ ! -f "$src" ]]; then
      echo "error: expected binary not found: $src" >&2
      exit 1
    fi
    cp "$src" "$(staged_path "$entry")"
    echo "  staged $(basename "$(staged_path "$entry")")"
  done
  if [[ "$phase" == stage ]]; then
    echo "done (staged; run with --pack to finish)."
    exit 0
  fi
fi

# Portable SHA-256: sha256sum on Linux and Git Bash, shasum -a 256 on macOS.
sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

checksum_name="CHECKSUMS-$target.txt"
checksums="$out/$checksum_name"
: >"$checksums"

echo "== checksumming binaries =="
for entry in "${staged_bins[@]}"; do
  dest="$(staged_path "$entry")"
  if [[ ! -f "$dest" ]]; then
    echo "error: staged binary not found: $dest (run --stage first)" >&2
    exit 1
  fi
  printf '%s  %s\n' "$(sha256_of "$dest")" "$(basename "$dest")" >>"$checksums"
done

# The archive's README: what each file is for and how to verify it.
{
  echo "CretLink $version for $os ($arch)"
  echo
  echo "Files"
  echo
  echo "  cretlink$exe"
  echo "      The CretLink application. Start it to give help"
  if [[ "$os" == windows ]]; then
    echo "      or, on the computer that needs it, to get help. Renamed to"
    echo "      CretLink-Support.exe it opens Get help straight away."
  else
    echo "      (getting help needs Windows)."
  fi
  echo "      'cretlink mcp install' connects your AI assistants."
  echo "  cretlink-cli$exe"
  echo "      The same commands for a terminal, without a window; use it for"
  echo "      'shell' on Windows. The CretLink command line. 'cretlink-cli mcp install' registers"
  echo "      CretLink with Claude Code, Codex CLI and Gemini CLI;"
  echo "      'cretlink-cli mcp status' shows where it is registered;"
  echo "      'cretlink-cli shell' opens a terminal on the consented session;"
  echo "      'cretlink-cli version' prints the version."
  echo "  cretlink-console$exe"
  echo "      The technician console that pairs with the customer's computer."
  echo "  cretlink-mcp$exe, cretlink-shell$exe"
  echo "      The MCP bridge and terminal client as separate programs; they"
  echo "      behave like 'cretlink-cli mcp' and 'cretlink-cli shell'."
  if [[ "$os" == windows ]]; then
    echo "  cretlink-support-agent$exe"
    echo "      The program the customer runs on the Windows computer that needs"
    echo "      help. It asks for administrator rights and for consent."
  fi
  echo "  $checksum_name"
  echo "      SHA-256 checksums of the programs built for this platform."
  echo "  LICENSE"
  echo "      The MIT license."
  echo
  echo "Getting started"
  echo
  echo "  1. Keep this folder somewhere only you can write to."
  echo "  2. Run 'cretlink mcp install' (or 'cretlink-cli mcp install') to"
  echo "     connect your AI clients."
  echo "  3. Start cretlink, choose Give help, and connect to the customer's"
  echo "     computer."
  echo
  echo "Verifying"
  echo
  echo "  Compare this archive with SHA256SUMS.txt from the GitHub release, and"
  echo "  the programs with the CHECKSUMS file in this folder. How to check the"
  echo "  build provenance:"
  echo "  https://github.com/yveslaurentcreton/CretLink/blob/main/docs/verifying-artifacts.md"
} >"$stage/README.txt"
cp "$repo_root/LICENSE" "$stage/LICENSE"
cp "$checksums" "$stage/$checksum_name"

echo "== packing $bundle_name =="
(
  cd "$(dirname "$stage")"
  if [[ "$os" == windows ]]; then
    archive="$out/$bundle_name.zip"
    rm -f "$archive"
    if command -v 7z >/dev/null 2>&1; then
      7z a -tzip -bso0 "$archive" "$bundle_name" >/dev/null
    elif command -v zip >/dev/null 2>&1; then
      zip -qr "$archive" "$bundle_name"
    else
      powershell -NoProfile -Command \
        "Compress-Archive -Path '$(cygpath -w "$stage")' -DestinationPath '$(cygpath -w "$archive")'"
    fi
  else
    archive="$out/$bundle_name.tar.gz"
    tar -czf "$archive" "$bundle_name"
  fi
  echo "  wrote $(basename "$archive")"
)

# Dependency inventory — always produced, needs no extra tooling. The workspace
# lockfile pins every version; this records the resolved normal-dependency graph
# a released binary was built from (REQ-N-016 provenance).
inventory="$out/dependency-inventory-$target.txt"
{
  echo "CretLink dependency inventory"
  echo "version: $version"
  echo "target:  $target"
  echo "generated from the locked workspace (cargo --locked)."
  echo
  echo "== resolved normal and build dependency graph of the shipped packages (cargo tree) =="
  cargo tree --edges normal,build --locked "${build_args[@]}"
  echo
  echo "== the application (detached crate $app_dir/src-tauri, cargo tree) =="
  cargo tree --edges normal,build --locked --manifest-path "$app_dir/src-tauri/Cargo.toml"
  echo
  echo "== the application frontend (npm, locked by $app_dir/package-lock.json) =="
  (cd "$app_dir" && npm ls --omit=dev --all 2>/dev/null || true)
} >"$inventory"
echo "  wrote $(basename "$inventory")"

# Sets the version of the SBOM's own component, which cargo-cyclonedx takes
# from the manifest (`0.0.0`; semantic-release owns versions).
stamp_sbom() {
  if ! command -v node >/dev/null 2>&1; then
    echo "  warning: node not found; $(basename "$1") keeps the manifest version" >&2
    return
  fi
  node -e '
    const fs = require("fs");
    const [file, version] = process.argv.slice(1);
    const sbom = JSON.parse(fs.readFileSync(file, "utf8"));
    if (sbom.metadata && sbom.metadata.component) {
      sbom.metadata.component.version = version;
    }
    fs.writeFileSync(file, JSON.stringify(sbom, null, 2) + "\n");
  ' "$1" "$version"
}

# CycloneDX SBOMs — machine-readable provenance, one per shipped binary.
# cargo-cyclonedx writes <name>.cdx.json next to each workspace member's
# Cargo.toml (the workspace root is virtual), filtered to the build target; the
# shipped packages' files are collected and every stray file is removed.
require_sbom="${CRETLINK_REQUIRE_SBOM:-0}"
if cargo cyclonedx --help >/dev/null 2>&1; then
  echo "== generating CycloneDX SBOMs =="
  find "$repo_root" -path "$repo_root/target" -prune -o -path "$repo_root/$app_dir/src-tauri/target" -prune \
    -o -name node_modules -prune -o -name '*.cdx.json' -print0 | xargs -0 rm -f
  cargo cyclonedx --format json --target "$target" --override-filename cretlink-sbom
  missing=0
  for entry in "${bins[@]}"; do
    bin="$(field "$entry" 1)"
    produced="$repo_root/$(field "$entry" 3)/cretlink-sbom.json"
    if [[ -f "$produced" ]]; then
      sbom="$out/sbom-$bin-$target.cdx.json"
      mv "$produced" "$sbom"
      stamp_sbom "$sbom"
      echo "  wrote $(basename "$sbom")"
    else
      echo "  error: no SBOM produced for $(field "$entry" 2)" >&2
      missing=1
    fi
  done
  # The application is a detached crate: its SBOM comes from its own manifest.
  (cd "$app_dir/src-tauri" && cargo cyclonedx --format json --target "$target" \
    --override-filename cretlink-sbom)
  if [[ -f "$app_dir/src-tauri/cretlink-sbom.json" ]]; then
    sbom="$out/sbom-cretlink-$target.cdx.json"
    mv "$app_dir/src-tauri/cretlink-sbom.json" "$sbom"
    stamp_sbom "$sbom"
    echo "  wrote $(basename "$sbom")"
  else
    echo "  error: no SBOM produced for cretlink-app" >&2
    missing=1
  fi
  find "$repo_root" -path "$repo_root/target" -prune -o -path "$repo_root/$app_dir/src-tauri/target" -prune \
    -o -name node_modules -prune -o -name 'cretlink-sbom.json' -print0 | xargs -0 rm -f
  if [[ "$missing" == 1 && "$require_sbom" == 1 ]]; then
    exit 1
  fi
elif [[ "$require_sbom" == 1 ]]; then
  echo "error: cargo-cyclonedx is required for official artifacts" >&2
  exit 1
else
  echo "== CycloneDX SBOMs skipped: cargo-cyclonedx not installed =="
  echo "  (the release workflow installs it; the dependency inventory is still written.)"
fi

echo
echo "== artifacts for $target =="
ls -1 "$out" | sed 's/^/  /'
echo "done."
