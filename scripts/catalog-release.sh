#!/bin/sh
set -eu

usage() {
    cat <<'EOF'
Publish a locally generated Oracle catalog as a GitHub Release asset.

Usage:
  scripts/catalog-release.sh --release RELEASE --database PATH
EOF
}

release=
database=
while [ "$#" -gt 0 ]; do
    case "$1" in
        --release)
            [ "$#" -ge 2 ] || { echo "error: --release requires a value" >&2; exit 2; }
            release=$2
            shift 2
            ;;
        --database)
            [ "$#" -ge 2 ] || { echo "error: --database requires a value" >&2; exit 2; }
            database=$2
            shift 2
            ;;
        -h|--help) usage; exit 0 ;;
        *) echo "error: unknown option: $1" >&2; usage >&2; exit 2 ;;
    esac
done

[ -n "$release" ] || { echo "error: --release is required" >&2; exit 2; }
[ -n "$database" ] || { echo "error: --database is required" >&2; exit 2; }
[ -f "$database" ] || { echo "error: database does not exist: $database" >&2; exit 1; }
command -v gh >/dev/null 2>&1 || { echo "error: gh is required" >&2; exit 1; }
command -v gzip >/dev/null 2>&1 || { echo "error: gzip is required" >&2; exit 1; }
command -v python3 >/dev/null 2>&1 || { echo "error: python3 is required" >&2; exit 1; }

release=$(printf '%s' "$release" | tr '[:lower:]' '[:upper:]')
case "$release" in
    *[!0-9A-Z]*) echo "error: invalid Oracle release: $release" >&2; exit 2 ;;
esac

temporary_directory=$(mktemp -d "${TMPDIR:-/tmp}/oracle-catalog.XXXXXX")
trap 'rm -rf "$temporary_directory"' EXIT HUP INT TERM

asset="catalog-${release}.sqlite.gz"
gzip -9 -c "$database" > "$temporary_directory/$asset"

if command -v shasum >/dev/null 2>&1; then
    checksum=$(shasum -a 256 "$temporary_directory/$asset" | awk '{print $1}')
else
    checksum=$(sha256sum "$temporary_directory/$asset" | awk '{print $1}')
fi

RELEASE="$release" ASSET="$asset" CHECKSUM="$checksum" \
    python3 - "$temporary_directory/manifest.json" <<'PY'
import json
import os
import sys
from pathlib import Path

Path(sys.argv[1]).write_text(json.dumps({
    "release": os.environ["RELEASE"],
    "asset": os.environ["ASSET"],
    "sha256": os.environ["CHECKSUM"],
}, indent=2) + "\n")
PY

printf '%s  %s\n' "$checksum" "$asset" > "$temporary_directory/SHA256SUMS"
tag="catalog-${release}"
if gh release view "$tag" >/dev/null 2>&1; then
    gh release upload "$tag" \
        "$temporary_directory/$asset" \
        "$temporary_directory/manifest.json" \
        "$temporary_directory/SHA256SUMS" \
        --clobber
else
    gh release create "$tag" \
        "$temporary_directory/$asset" \
        "$temporary_directory/manifest.json" \
        "$temporary_directory/SHA256SUMS" \
        --target "$(git rev-parse HEAD)" \
        --title "Oracle catalog $release" \
        --notes "Pre-generated Oracle Fusion ERP catalog for release $release."
fi
