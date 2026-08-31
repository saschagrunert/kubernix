#!/usr/bin/env bash
set -euo pipefail

for cmd in nix jq; do
    command -v "$cmd" >/dev/null 2>&1 || {
        echo "error: $cmd is required but not found" >&2
        exit 1
    }
done

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
README="$ROOT_DIR/README.md"
README_TMP="$README.tmp"

trap 'rm -f "$README_TMP"' EXIT

# Display-name overrides (nix pname -> README name).
# These map nixpkgs internal pname values, not the attribute names in
# packages.nix. If nixpkgs changes a pname, update this map.
declare -A NAME_MAP=(
    [cri-o]=cri-o-wrapper
    [procps]=sysctl
)

# Packages excluded from the versions table
declare -A EXCLUDE=(
    [jq]=1
)

# Detect the current system architecture
SYSTEM="$(nix eval --raw --impure --expr 'builtins.currentSystem')"

# Query nix for all package versions from the devShell
VERSIONS=$(nix eval --json ".#devShells.${SYSTEM}.default.buildInputs" \
    --apply 'builtins.map (p: { name = p.pname or p.name; version = let v = p.version or ""; in if v == "" then "unknown" else v; })')

# Apply name mapping and exclusions, then sort by display name
ROWS=$(while IFS=$'\t' read -r name version; do
    [[ -n "${EXCLUDE[$name]:-}" ]] && continue
    display_name="${NAME_MAP[$name]:-$name}"
    printf '%s\tv%s\n' "$display_name" "$version"
done < <(echo "$VERSIONS" | jq -r '.[] | [.name, .version] | @tsv') | sort)

if [[ -z "$ROWS" ]]; then
    echo "error: no packages found in devShell" >&2
    exit 1
fi

# Compute column widths (seeded from header text)
HEADER_NAME="Application"
HEADER_VER="Version"
NAME_WIDTH=${#HEADER_NAME}
VERSION_WIDTH=${#HEADER_VER}
while IFS=$'\t' read -r name version; do
    ((${#name} > NAME_WIDTH)) && NAME_WIDTH=${#name}
    ((${#version} > VERSION_WIDTH)) && VERSION_WIDTH=${#version}
done <<< "$ROWS"

# Build the table with dynamic widths
SEP_NAME=$(printf '%*s' "$NAME_WIDTH" '' | tr ' ' '-')
SEP_VER=$(printf '%*s' "$VERSION_WIDTH" '' | tr ' ' '-')

TABLE=$(
    printf '| %-*s | %-*s |' "$NAME_WIDTH" "$HEADER_NAME" "$VERSION_WIDTH" "$HEADER_VER"
    printf '\n| %s | %s |' "$SEP_NAME" "$SEP_VER"
    while IFS=$'\t' read -r display_name version_str; do
        printf '\n| %-*s | %-*s |' "$NAME_WIDTH" "$display_name" "$VERSION_WIDTH" "$version_str"
    done <<< "$ROWS"
)

# Replace the table in README.md between the markers
awk -v table="$TABLE" '
    /^\| Application/ { skip=1; print table; next }
    skip && /^\|/ { next }
    skip { skip=0 }
    { print }
' "$README" > "$README_TMP"

mv "$README_TMP" "$README"
echo "Updated versions table in README.md"
