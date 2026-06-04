#!/usr/bin/env bash
# Local release pipeline: cargo build -> Glass.app -> Glass.dmg.
#
# This is the entry point — call it directly. The lower-level
# scripts (make-app.sh, make-dmg.sh) are usable on their own when
# iterating on packaging, but for a clean "I want a shippable
# DMG" run, use this.
#
# Usage:  packaging/build-release.sh
#
# Output:  dist/Glass.app  +  dist/Glass-<version>-<arch>.dmg
#
# Local-only: produces an ad-hoc signed bundle. Distributing to
# other machines without a Gatekeeper warning needs the
# Developer-ID-signed + notarized pipeline (the CI workflow that
# this script will eventually be lifted into).

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${REPO_ROOT}"

echo "==> cargo build --release -p glass-cli"
cargo build --release -p glass-cli

echo "==> packaging Glass.app"
"${REPO_ROOT}/packaging/make-app.sh"

echo "==> building DMG"
"${REPO_ROOT}/packaging/make-dmg.sh"

echo
echo "done. artefacts in ${REPO_ROOT}/dist/"
ls -lh "${REPO_ROOT}/dist/"
