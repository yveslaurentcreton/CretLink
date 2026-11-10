#!/usr/bin/env bash
# Fail when a shipped Linux binary needs a newer glibc than the baseline
# (ST-M009-013, REQ-N-021: Linux builds run on glibc 2.35 or newer).
#
# Usage: scripts/check-glibc.sh [--max VERSION] PATH...
#
# Every PATH is an ELF binary, a directory (every ELF file in it is checked,
# recursively) or a .tar.gz / .tgz archive (unpacked to a temporary directory
# and checked the same way). The highest `GLIBC_x.y` symbol version each binary
# needs is read from its version-needs section with `readelf` (binutils), or
# with `objdump` when readelf is missing.
#
# VERSION defaults to 2.35, or CRETLINK_GLIBC_MAX when that is set.
#
# Exit status: 0 when every binary stays within the baseline; 1 when any needs
# a newer GLIBC_ version (the offending symbols are listed); 2 on a usage error,
# a missing tool, an unreadable path, or when no ELF binary was found at all, so
# an empty or wrong argument never passes.
set -euo pipefail

max="${CRETLINK_GLIBC_MAX:-2.35}"
usage() {
  echo "usage: $0 [--max VERSION] PATH..." >&2
  exit 2
}

paths=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --max)
      [[ $# -ge 2 ]] || usage
      max="$2"
      shift 2
      ;;
    --max=*) max="${1#--max=}"; shift ;;
    -h | --help) usage ;;
    --) shift; paths+=("$@"); break ;;
    -*) echo "error: unknown option $1" >&2; usage ;;
    *) paths+=("$1"); shift ;;
  esac
done
[[ ${#paths[@]} -gt 0 ]] || usage
if [[ ! "$max" =~ ^[0-9]+\.[0-9]+(\.[0-9]+)?$ ]]; then
  echo "error: '$max' is not a glibc version" >&2
  exit 2
fi

if command -v readelf >/dev/null 2>&1; then
  tool=readelf
elif command -v objdump >/dev/null 2>&1; then
  tool=objdump
else
  echo "error: neither readelf nor objdump found; install binutils" >&2
  exit 2
fi

# The GLIBC_ versions a binary needs, one per line (GLIBC_PRIVATE and the
# like are not numbered and are ignored).
needed_versions() {
  local dump
  if [[ "$tool" == readelf ]]; then
    dump="$(LC_ALL=C readelf --version-info --wide "$1")" || return 1
  else
    dump="$(LC_ALL=C objdump -p "$1")" || return 1
  fi
  { grep -oE '\bGLIBC_[0-9]+(\.[0-9]+)+\b' <<<"$dump" || true; } | sed 's/^GLIBC_//' | sort -uV
}

# The dynamic symbols bound to a given GLIBC_ version, for the failure report.
symbols_at() { # symbols_at <file> <version>
  if [[ "$tool" == readelf ]]; then
    LC_ALL=C readelf --dyn-syms --wide "$1" | awk -v v="@GLIBC_$2" '
      { for (i = 1; i <= NF; i++) { n = index($i, v);
          if (n > 0 && substr($i, n + length(v)) ~ /^( \([0-9]+\))?$/) {
            print substr($i, 1, n - 1) } } }'
  else
    LC_ALL=C objdump -T "$1" | awk -v v="GLIBC_$2" '
      { for (i = 1; i < NF; i++) { f = $i; gsub(/[()]/, "", f);
          if (f == v) { print $NF } } }'
  fi | sort -u
}

# Whether version $1 is newer than version $2.
newer_than() {
  [[ "$1" != "$2" && "$(printf '%s\n%s\n' "$1" "$2" | sort -V | tail -n1)" == "$1" ]]
}

is_elf() {
  [[ -f "$1" && "$(LC_ALL=C head -c 4 "$1" 2>/dev/null | od -An -tx1 | tr -d ' \n')" == 7f454c46 ]]
}

tmp_dirs=()
cleanup() {
  if [[ ${#tmp_dirs[@]} -gt 0 ]]; then
    rm -rf "${tmp_dirs[@]}"
  fi
}
trap cleanup EXIT

checked=0
failed=0
check_file() { # check_file <file> <label>
  local file="$1" label="$2" versions highest
  checked=$((checked + 1))
  if ! versions="$(needed_versions "$file")"; then
    echo "error: $tool cannot read $label" >&2
    exit 2
  fi
  if [[ -z "$versions" ]]; then
    echo "  ok    $label (needs no versioned glibc symbol)"
    return
  fi
  highest="$(tail -n1 <<<"$versions")"
  if newer_than "$highest" "$max"; then
    failed=1
    echo "  FAIL  $label needs GLIBC_$highest (baseline $max)"
    local v
    while read -r v; do
      if newer_than "$v" "$max"; then
        echo "        GLIBC_$v: $(symbols_at "$file" "$v" | tr '\n' ' ')"
      fi
    done <<<"$versions"
  else
    echo "  ok    $label needs GLIBC_$highest"
  fi
}

check_tree() { # check_tree <directory> <label prefix>
  local dir="$1" prefix="$2" file
  while IFS= read -r -d '' file; do
    if is_elf "$file"; then
      check_file "$file" "$prefix${file#"$dir"/}"
    fi
  done < <(find "$dir" -type f -print0 | sort -z)
}

echo "== glibc baseline $max ($tool) =="
for path in "${paths[@]}"; do
  if [[ -d "$path" ]]; then
    check_tree "$path" "$path/"
  elif [[ -f "$path" && ("$path" == *.tar.gz || "$path" == *.tgz) ]]; then
    dir="$(mktemp -d)"
    tmp_dirs+=("$dir")
    if ! tar -xzf "$path" -C "$dir"; then
      echo "error: cannot unpack $path" >&2
      exit 2
    fi
    check_tree "$dir" "$(basename "$path"):"
  elif is_elf "$path"; then
    check_file "$path" "$path"
  elif [[ -e "$path" ]]; then
    echo "error: $path is not an ELF binary, a directory or a .tar.gz archive" >&2
    exit 2
  else
    echo "error: $path not found" >&2
    exit 2
  fi
done

if [[ "$checked" -eq 0 ]]; then
  echo "error: no ELF binary found in ${paths[*]}" >&2
  exit 2
fi
if [[ "$failed" -ne 0 ]]; then
  echo "error: a binary needs a glibc newer than $max (REQ-N-021)" >&2
  exit 1
fi
echo "ok: $checked ELF binaries checked, none needs a glibc newer than $max."
