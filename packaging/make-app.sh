#!/usr/bin/env bash
# Wrap the release binary in a Glass.app bundle.
#
# Local development build: ad-hoc signed only. Real Developer ID
# signing + notarization happens in the CI release workflow (see
# the eventual .github/workflows/release.yml).
#
# Usage:  packaging/make-app.sh [OUT_DIR]
#   OUT_DIR defaults to ./dist; result is ${OUT_DIR}/Glass.app
#
# Pre-requisite: a release binary at target/release/glass. Run
# `cargo build --release -p glass-cli` first, or use the
# packaging/build-release.sh orchestrator which does both steps.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_DIR="${1:-${REPO_ROOT}/dist}"
APP_DIR="${OUT_DIR}/Glass.app"
BIN="${REPO_ROOT}/target/release/glass"
PLIST_TEMPLATE="${REPO_ROOT}/packaging/Info.plist"
ICON_SRC="${REPO_ROOT}/packaging/Glass.icns"

if [[ ! -x "${BIN}" ]]; then
    echo "release binary not found at ${BIN}" >&2
    echo "build first:  cargo build --release -p glass-cli" >&2
    exit 1
fi

# Pull the canonical workspace version out of Cargo.toml so the
# plist tracks `cargo` rather than drifting. Falls back to
# whatever's already in Info.plist if grep can't find it (e.g.
# someone's restructured workspace.package).
VERSION="$(awk '/^\[workspace\.package\]/{flag=1; next} /^\[/{flag=0} flag && /^version/ {gsub(/[" ]/,"",$3); print $3; exit}' "${REPO_ROOT}/Cargo.toml")"
if [[ -z "${VERSION}" ]]; then
    echo "couldn't read workspace.package.version from Cargo.toml" >&2
    exit 1
fi

# Clean rebuild — `cp`-ing into a stale .app leaves orphan files
# (especially under Resources/) that survive across runs.
rm -rf "${APP_DIR}"
mkdir -p "${APP_DIR}/Contents/MacOS"
mkdir -p "${APP_DIR}/Contents/Resources"

cp "${BIN}" "${APP_DIR}/Contents/MacOS/glass"

# Stamp the version into the plist as we copy it. Same value goes
# into CFBundleVersion (build number — Apple wants monotonic) and
# CFBundleShortVersionString (marketing — semver). For CI we'll
# probably grow CFBundleVersion to include a build counter.
sed \
    -e "s|@@CFBUNDLE_VERSION@@|${VERSION}|g" \
    -e "s|@@CFBUNDLE_SHORT_VERSION@@|${VERSION}|g" \
    "${PLIST_TEMPLATE}" > "${APP_DIR}/Contents/Info.plist"

# Drop in the icon if one's been authored. Soft-fail when missing
# so the local script still produces a runnable bundle before the
# icon exists.
if [[ -f "${ICON_SRC}" ]]; then
    cp "${ICON_SRC}" "${APP_DIR}/Contents/Resources/Glass.icns"
else
    echo "note: no icon at ${ICON_SRC} — bundle will use the default" >&2
fi

# Ad-hoc sign so Gatekeeper at least recognises the bundle
# structure (still unsigned for distribution). The `--deep` is
# legal here because we're only signing our own executable and
# the bundle wrapper — no nested frameworks.
codesign --force --deep --sign - "${APP_DIR}" >/dev/null

# Sanity check: the bundle should be valid even ad-hoc signed.
if ! codesign --verify --strict "${APP_DIR}" 2>/dev/null; then
    echo "warning: codesign --verify failed on ${APP_DIR}" >&2
fi

echo "built ${APP_DIR} (version ${VERSION})"
