#!/usr/bin/env bash
# Wrap the release binary in a Glass.app bundle.
#
# Usage:  packaging/make-app.sh [OUT_DIR]
#   OUT_DIR defaults to ./dist; result is ${OUT_DIR}/Glass.app

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_DIR="${1:-${REPO_ROOT}/dist}"
APP_DIR="${OUT_DIR}/Glass.app"
BIN="${REPO_ROOT}/target/release/glass"

if [[ ! -x "${BIN}" ]]; then
    echo "release binary not found at ${BIN}" >&2
    echo "build first:  cargo build --release -p glass-cli" >&2
    exit 1
fi

rm -rf "${APP_DIR}"
mkdir -p "${APP_DIR}/Contents/MacOS"
mkdir -p "${APP_DIR}/Contents/Resources"

cp "${BIN}" "${APP_DIR}/Contents/MacOS/glass"
cp "${REPO_ROOT}/packaging/Info.plist" "${APP_DIR}/Contents/Info.plist"

# Ad-hoc sign so Gatekeeper at least recognises the bundle structure
# (the bundle is still unsigned for distribution purposes — see the
# CI workflow / README for why first-launch needs right-click open).
codesign --force --sign - "${APP_DIR}/Contents/MacOS/glass" 2>/dev/null || true
codesign --force --sign - "${APP_DIR}" 2>/dev/null || true

echo "built ${APP_DIR}"
