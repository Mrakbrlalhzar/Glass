#!/usr/bin/env bash
# Package a built Glass.app into a distributable .dmg.
#
# Uses `hdiutil` only — no third-party deps (create-dmg etc.).
# Layout is intentionally plain: the app and an /Applications
# symlink, no styled background. We can layer a designed
# background + window geometry on later once we have an icon.
#
# Usage:  packaging/make-dmg.sh [DIST_DIR]
#   DIST_DIR defaults to ./dist; expects ${DIST_DIR}/Glass.app to
#   already exist (run packaging/make-app.sh first, or use the
#   build-release.sh orchestrator).

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIST_DIR="${1:-${REPO_ROOT}/dist}"
APP_DIR="${DIST_DIR}/Glass.app"

if [[ ! -d "${APP_DIR}" ]]; then
    echo "no Glass.app at ${APP_DIR}" >&2
    echo "build first:  packaging/make-app.sh" >&2
    exit 1
fi

VERSION="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "${APP_DIR}/Contents/Info.plist")"
ARCH="$(uname -m)"  # arm64 on Apple Silicon; matches CI artifact naming.
DMG_NAME="Glass-${VERSION}-${ARCH}.dmg"
DMG_PATH="${DIST_DIR}/${DMG_NAME}"

# Staging dir laid out the way the DMG should appear when mounted:
# Glass.app sits next to a symlink labelled "Applications" so the
# user can drag-and-drop in one motion. hdiutil bakes whatever's
# in this dir into the image.
STAGE_DIR="$(mktemp -d -t glass-dmg)"
trap 'rm -rf "${STAGE_DIR}"' EXIT

cp -R "${APP_DIR}" "${STAGE_DIR}/Glass.app"
ln -s /Applications "${STAGE_DIR}/Applications"

# Remove any prior DMG at the same path — hdiutil refuses to
# overwrite, and a stale image would otherwise wedge the script.
rm -f "${DMG_PATH}"

# UDZO = compressed read-only. Volume name "Glass" is what shows
# up under /Volumes when the user mounts the image. The format
# matches what notarytool accepts directly, so the CI flow can
# notarize the DMG itself rather than a wrapping zip.
hdiutil create \
    -volname "Glass" \
    -srcfolder "${STAGE_DIR}" \
    -ov \
    -format UDZO \
    "${DMG_PATH}" >/dev/null

SIZE="$(du -h "${DMG_PATH}" | cut -f1)"
echo "built ${DMG_PATH} (${SIZE})"
