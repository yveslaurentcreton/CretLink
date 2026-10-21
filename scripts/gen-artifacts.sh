#!/usr/bin/env bash
# Generate verifiable release artifacts for CretLink (ST-M006-004).
#
# Portable across the release matrix (windows / macos / ubuntu runners, all of
# which provide a bash shell) and runnable locally in any bash (Linux, macOS, or
# Git Bash on Windows), so a maintainer — or CI — produces the same artifact
# metadata from the same commit.
#
# Under dist/ it writes, for the host target triple:
#   - the five release binaries, renamed <bin>-<target>[.exe];
#   - CHECKSUMS-<target>.txt   SHA-256 over every binary for this target;
#   - dependency-inventory-<target>.txt   the exact normal and build dependency
#     graph and pinned versions of the shipped packages (cargo tree, locked);
#   - sbom-<binary>-<target>.cdx.json   a CycloneDX SBOM per shipped binary,
#     when cargo-cyclonedx is installed. The release workflow installs it and
#     sets CRETLINK_REQUIRE_SBOM=1, so official artifacts fail rather than ship
#     without SBOMs; locally a missing tool is reported, never faked.
#
# The binary filenames carry the *target*, not the version: the release tag and
# GitHub release semantic-release creates carry the version (REQ-N-018), so this
# script never needs — and never chooses — a version number. An optional VERSION
# (arg 1 or env) is recorded only as inventory metadata to label a local build.
set -euo pipefail

version="${1:-${VERSION:-0.0.0-dev}}"
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

# Host target triple, straight from the compiler that will build the binaries.
target="$(rustc -vV | sed -n 's/^host: //p')"
case "$target" in
  *windows*) exe=".exe" ;;
  *)         exe="" ;;
esac

out="$repo_root/dist"
mkdir -p "$out"

# The five shippable binaries and their owning packages. The console, MCP
# bridge, and host shell run on the technician computer; the support agent is
# the only binary ever placed on the customer computer; the relay server runs on
# neither endpoint — it is operator-hosted public infrastructure that forwards
# opaque end-to-end Noise sessions and holds no session key (ST-M007-007).
# Each entry is <binary>:<package>:<package directory>.
bins=(
  "cretlink-console:cretlink-console:apps/console"
  "cretlink-mcp:cretlink-mcp-bridge:apps/mcp-bridge"
  "cretlink-shell:cretlink-shell:apps/shell-cli"
  "cretlink-support-agent:cretlink-support-agent:apps/support-agent"
  "cretlink-relay-server:cretlink-relay-server:apps/relay-server"
)

echo "== building release binaries for $target (label $version) =="
build_args=()
for entry in "${bins[@]}"; do
  pkg="${entry#*:}"
  build_args+=("-p" "${pkg%%:*}")
done
cargo build --release --locked "${build_args[@]}"

# Portable SHA-256: sha256sum on Linux and Git Bash, shasum -a 256 on macOS.
sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

checksums="$out/CHECKSUMS-$target.txt"
: >"$checksums"

echo "== staging + checksumming artifacts =="
for entry in "${bins[@]}"; do
  bin="${entry%%:*}"
  src="$repo_root/target/release/$bin$exe"
  if [[ ! -f "$src" ]]; then
    echo "error: expected binary not found: $src" >&2
    exit 1
  fi
  dest_name="$bin-$target$exe"
  cp "$src" "$out/$dest_name"
  printf '%s  %s\n' "$(sha256_of "$out/$dest_name")" "$dest_name" >>"$checksums"
  echo "  staged $dest_name"
done

# Dependency inventory — always produced, needs no extra tooling. The workspace
# lockfile pins every version; this records the resolved normal-dependency graph
# a released binary was built from (REQ-N-016 provenance).
inventory="$out/dependency-inventory-$target.txt"
{
  echo "CretLink dependency inventory"
  echo "label:  $version"
  echo "target: $target"
  echo "generated from the locked workspace (cargo --locked)."
  echo
  echo "== resolved normal and build dependency graph of the shipped packages (cargo tree) =="
  cargo tree --edges normal,build --locked "${build_args[@]}"
} >"$inventory"
echo "  wrote $(basename "$inventory")"

# CycloneDX SBOMs — machine-readable provenance, one per shipped binary.
# cargo-cyclonedx writes <name>.cdx.json next to each workspace member's
# Cargo.toml (the workspace root is virtual), filtered to the build target; the
# shipped packages' files are collected and every stray file is removed.
require_sbom="${CRETLINK_REQUIRE_SBOM:-0}"
if cargo cyclonedx --help >/dev/null 2>&1; then
  echo "== generating CycloneDX SBOMs =="
  find "$repo_root" -path "$repo_root/target" -prune -o -name '*.cdx.json' -print0 | xargs -0 rm -f
  cargo cyclonedx --format json --target "$target" --override-filename cretlink-sbom
  missing=0
  for entry in "${bins[@]}"; do
    bin="${entry%%:*}"
    pkg="${entry#*:}"
    pkg="${pkg%%:*}"
    produced="$repo_root/${entry##*:}/cretlink-sbom.json"
    if [[ -f "$produced" ]]; then
      mv "$produced" "$out/sbom-$bin-$target.cdx.json"
      echo "  wrote sbom-$bin-$target.cdx.json"
    else
      echo "  error: no SBOM produced for $pkg" >&2
      missing=1
    fi
  done
  find "$repo_root" -path "$repo_root/target" -prune -o -name 'cretlink-sbom.json' -print0 | xargs -0 rm -f
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
