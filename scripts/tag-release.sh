#!/usr/bin/env bash
# Create and push a release tag: v{Cargo.toml version}-{7-char-sha}
# Matches .github/workflows/release.yml
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

DRY=0
if [[ "${1:-}" == "-n" || "${1:-}" == "--dry-run" ]]; then
  DRY=1
fi

if [[ "$DRY" -eq 0 && -n "$(git status --porcelain)" ]]; then
  echo "error: working tree not clean" >&2
  exit 1
fi

PKG="$(awk '
  $0 == "[workspace.package]" { in_wp=1; next }
  in_wp && /^\[/ { exit }
  in_wp && $1 == "version" {
    gsub(/"/, "", $3); print $3; exit
  }
' Cargo.toml)"
if [[ -z "${PKG}" ]]; then
  echo "error: could not read workspace.package.version" >&2
  exit 1
fi

SHA="$(git rev-parse --short=7 HEAD)"
TAG="v${PKG}-${SHA}"

if git rev-parse -q --verify "refs/tags/${TAG}" >/dev/null; then
  echo "error: tag already exists: ${TAG}" >&2
  exit 1
fi

REMOTE="${RELEASE_REMOTE:-}"
if [[ -z "${REMOTE}" ]]; then
  if git remote get-url public &>/dev/null; then
    REMOTE=public
  else
    REMOTE=origin
  fi
fi

if [[ "$DRY" -eq 1 ]]; then
  echo "would tag and push: ${TAG} → ${REMOTE}"
  exit 0
fi

git tag "${TAG}"
echo "created ${TAG}"
git push "${REMOTE}" "${TAG}"
echo "pushed ${TAG} → ${REMOTE}"
