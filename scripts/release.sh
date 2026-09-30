#!/bin/sh
set -eu

usage() {
    cat <<'EOF'
Create a production release by updating Cargo, tagging, and pushing.

Usage:
  scripts/release.sh --bump patch|minor|major [OPTIONS]

Examples:
  scripts/release.sh --bump patch
  scripts/release.sh --bump minor --yes
  scripts/release.sh --bump major --dry-run
EOF
}

bump=
dry_run=false
assume_yes=false
while [ "$#" -gt 0 ]; do
    case "$1" in
        --dry-run) dry_run=true; shift ;;
        --yes|-y) assume_yes=true; shift ;;
        --bump)
            [ "$#" -ge 2 ] || {
                echo "error: --bump requires patch, minor, or major" >&2
                exit 2
            }
            [ -z "$bump" ] || {
                echo "error: only one bump type is allowed" >&2
                exit 2
            }
            bump=$2
            case "$bump" in
                patch|minor|major) ;;
                *) echo "error: --bump must be patch, minor, or major" >&2; exit 2 ;;
            esac
            shift 2
            ;;
        -h|--help) usage; exit 0 ;;
        *) echo "error: use --bump patch|minor|major" >&2; exit 2 ;;
    esac
done

[ -n "$bump" ] || {
    echo "error: --bump is required" >&2
    usage >&2
    exit 2
}

command -v git >/dev/null 2>&1 || {
    echo "error: git is required" >&2
    exit 1
}
command -v python3 >/dev/null 2>&1 || {
    echo "error: python3 is required" >&2
    exit 1
}

branch=$(git branch --show-current)
[ "$branch" = "main" ] || {
    echo "error: releases must start from main, current branch is $branch" >&2
    exit 1
}

git diff --quiet || {
    echo "error: working tree has unstaged changes" >&2
    exit 1
}
git diff --cached --quiet || {
    echo "error: index has staged changes" >&2
    exit 1
}

untracked=$(git ls-files --others --exclude-standard |
    grep -Ev '(^|/)[^/]+\.sqlite(-journal|-wal|-shm)?$' || true)
[ -z "$untracked" ] || {
    echo "error: working tree has untracked files:" >&2
    printf '%s\n' "$untracked" >&2
    exit 1
}

remote_branch=$(git rev-parse --abbrev-ref --symbolic-full-name '@{u}' 2>/dev/null || true)
[ "$remote_branch" = "origin/main" ] || {
    echo "error: main must track origin/main" >&2
    exit 1
}
# Binary releases use only semver tags; catalog-* tags are managed separately.
git fetch origin main 'refs/tags/v*:refs/tags/v*'
git diff --quiet HEAD origin/main || {
    echo "error: local main is not synchronized with origin/main" >&2
    exit 1
}

latest_tag=$(git tag --list 'v*' --sort=-v:refname | head -n 1)
cargo_version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n 1)
[ -n "$cargo_version" ] || {
    echo "error: could not read package version from Cargo.toml" >&2
    exit 1
}
if [ -n "$latest_tag" ]; then
    latest_version=${latest_tag#v}
    [ "$cargo_version" = "$latest_version" ] || {
        echo "error: Cargo.toml version ($cargo_version) differs from latest tag ($latest_tag)" >&2
        exit 1
    }
else
    latest_version=$cargo_version
fi

version=$(python3 - "$latest_version" "$bump" <<'PY'
import re
import sys

version, bump = sys.argv[1:]
match = re.fullmatch(r"(\d+)\.(\d+)\.(\d+)(?:[-.][0-9A-Za-z.-]+)?", version)
if not match:
    raise SystemExit(f"invalid base version: {version}")

major, minor, patch = map(int, match.groups())
if bump == "major":
    major, minor, patch = major + 1, 0, 0
elif bump == "minor":
    minor, patch = minor + 1, 0
else:
    patch += 1
print(f"{major}.{minor}.{patch}")
PY
)
tag="v$version"

printf 'Current version: %s\nNext version:    %s\nTag:             %s\n' \
    "$latest_version" "$version" "$tag"

if [ "$dry_run" = true ]; then
    echo "Dry run: would update Cargo.toml/Cargo.lock, commit, tag, and push."
    exit 0
fi

if [ "$assume_yes" != true ]; then
    printf 'Continue with release %s? [y/N] ' "$tag"
    read -r confirmation
    case "$confirmation" in
        y|Y|yes|YES) ;;
        *) echo "Release cancelled."; exit 0 ;;
    esac
fi

VERSION="$version" python3 - <<'PY'
import os
import re
from pathlib import Path

version = os.environ["VERSION"]
manifest = Path("Cargo.toml")
text = manifest.read_text()
text, count = re.subn(
    r'(?m)^version = "[^"]+"$',
    f'version = "{version}"',
    text,
    count=1,
)
if count != 1:
    raise SystemExit("could not update Cargo.toml package version")
manifest.write_text(text)

lock = Path("Cargo.lock")
lock_text = lock.read_text()
pattern = (
    r'(\[\[package\]\]\nname = "'
    + re.escape("oracle-fusion-erp-catalog-mcp")
    + r'"\nversion = ")[^"]+(")'
)
lock_text, count = re.subn(pattern, rf'\g<1>{version}\g<2>', lock_text, count=1)
if count != 1:
    raise SystemExit("could not update Cargo.lock package version")
lock.write_text(lock_text)
PY

git add Cargo.toml Cargo.lock
git commit -m "chore: release $tag"
git tag -a "$tag" -m "Release $tag"
git push origin main "$tag"
echo "Release tag $tag pushed; GitHub Actions will build and publish it."
