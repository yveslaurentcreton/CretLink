#!/usr/bin/env bash
# Start the Linux build on Debian 12 (glibc 2.36), a long-term release within
# the Linux baseline of glibc 2.35 or newer (ST-M009-013, REQ-N-021).
#
# Usage: scripts/smoke-debian.sh [--expect VERSION] PATH
#
# PATH is the Linux archive (.tar.gz) or a folder that holds `cretlink` and
# `cretlink-cli` the way the archive does. In a fresh `debian:12` container
# (CRETLINK_SMOKE_IMAGE overrides the image) it installs only the runtime
# libraries the application links (the webview, which pulls in GTK) and runs
# `cretlink version` and `cretlink-cli version` without a display: subcommands
# run before any window opens. With --expect, both must report that version.
#
# Needs docker and network access for apt. Exit status: 0 when both programs
# start and agree; 1 when either fails to start, prints no version or a
# different one; 2 on a usage error or a missing tool.
set -euo pipefail

image="${CRETLINK_SMOKE_IMAGE:-debian:12}"
expect=""
usage() {
  echo "usage: $0 [--expect VERSION] PATH" >&2
  exit 2
}
while [[ $# -gt 0 ]]; do
  case "$1" in
    --expect)
      [[ $# -ge 2 ]] || usage
      expect="$2"
      shift 2
      ;;
    -*) usage ;;
    *) break ;;
  esac
done
[[ $# -eq 1 ]] || usage
path="$1"

if ! command -v docker >/dev/null 2>&1; then
  echo "error: docker not found; the Debian 12 smoke test needs it" >&2
  exit 2
fi

tmp=""
trap '[[ -z "$tmp" ]] || rm -rf "$tmp"' EXIT
if [[ -d "$path" ]]; then
  root="$(cd "$path" && pwd)"
elif [[ -f "$path" && ("$path" == *.tar.gz || "$path" == *.tgz) ]]; then
  tmp="$(mktemp -d)"
  tar -xzf "$path" -C "$tmp"
  # The archive holds one folder named like the archive.
  root="$(find "$tmp" -mindepth 1 -maxdepth 1 -type d | head -n1)"
  [[ -n "$root" ]] || { echo "error: $path holds no folder" >&2; exit 1; }
else
  echo "error: $path is neither a folder nor a .tar.gz archive" >&2
  exit 2
fi
for bin in cretlink cretlink-cli; do
  if [[ ! -f "$root/$bin" ]]; then
    echo "error: $bin not found in $path" >&2
    exit 1
  fi
done

echo "== starting cretlink and cretlink-cli on $image =="
# The container has no display; DISPLAY and WAYLAND_DISPLAY are unset anyway so
# a subcommand that needed one would fail here.
if ! docker run --rm -e "EXPECT=$expect" -v "$root:/opt/cretlink:ro" "$image" sh -euc '
  unset DISPLAY WAYLAND_DISPLAY
  export DEBIAN_FRONTEND=noninteractive
  apt-get update -qq
  apt-get install -y -qq --no-install-recommends libwebkit2gtk-4.1-0 >/dev/null
  app="$(/opt/cretlink/cretlink version)"
  cli="$(/opt/cretlink/cretlink-cli version)"
  echo "cretlink version:     $app"
  echo "cretlink-cli version: $cli"
  [ -n "$app" ] && [ "${app##* }" = "${cli##* }" ] || { echo "error: the versions differ or are empty" >&2; exit 1; }
  if [ -n "$EXPECT" ] && [ "${app##* }" != "$EXPECT" ]; then
    echo "error: expected version $EXPECT" >&2
    exit 1
  fi
'; then
  echo "error: the smoke test failed on $image" >&2
  exit 1
fi
echo "ok: both programs start on $image."
