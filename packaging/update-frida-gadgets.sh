#!/usr/bin/env bash
# Refresh the bundled Frida gadget binaries to a pinned version.
#
# Glass vendors `libfrida-gadget.so` (Android) and
# `FridaGadget.dylib` (iOS) under
# `crates/glass-frida/assets/gadgets/`. This script downloads
# the matching tarballs from Frida's GitHub releases, verifies
# their SHA-256 against pins below, decompresses, and drops the
# binaries into place.
#
# Usage:  packaging/update-frida-gadgets.sh
#   Reads FRIDA_GADGET_VERSION below; pass --version <X.Y.Z> to
#   override on the command line (e.g. when testing a release
#   candidate).
#
# Why pin SHA-256s rather than blindly trust the download:
# we're going to compile these into Glass and ship them inside
# every patched APK we produce. A supply-chain incident on
# frida's CDN or GitHub releases would silently propagate. The
# pins live in this file so a bump is one PR that updates both
# the version and the hashes.

set -euo pipefail

# --- Pinned version + hashes -------------------------------------
# Bump these together. To update:
#   1. Pick the new version from https://github.com/frida/frida/releases.
#   2. Run this script with `--version <new>` (or update the
#      var below) — it'll fail the SHA check and print the
#      observed hash for each artifact.
#   3. Verify the observed hashes against Frida's release page
#      and paste them in here.
#   4. Re-run the script; it should succeed.
FRIDA_GADGET_VERSION="${FRIDA_GADGET_VERSION:-17.9.10}"

# SHA-256 of the .xz tarballs as published on frida's releases.
ANDROID_ARM64_XZ_SHA256="placeholder-fill-in-on-first-run"
IOS_UNIVERSAL_XZ_SHA256="placeholder-fill-in-on-first-run"

# --- Arg parsing -------------------------------------------------
while [[ $# -gt 0 ]]; do
    case "$1" in
        --version)
            FRIDA_GADGET_VERSION="$2"
            shift 2
            ;;
        --version=*)
            FRIDA_GADGET_VERSION="${1#*=}"
            shift
            ;;
        -h|--help)
            sed -n '1,/^set -e/p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "unknown argument: $1" >&2
            exit 2
            ;;
    esac
done

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
GADGET_DIR="${REPO_ROOT}/crates/glass-frida/assets/gadgets"
WORK_DIR="$(mktemp -d)"
trap 'rm -rf "${WORK_DIR}"' EXIT

echo "Fetching Frida gadgets v${FRIDA_GADGET_VERSION}…"

# --- Download helper --------------------------------------------
# Prints the observed SHA-256 if the pinned hash doesn't match.
# Hashes are tracked outside the file content so a length-only
# integrity check isn't enough — we want bit-for-bit.
download_and_verify() {
    local url="$1"
    local out="$2"
    local expected_sha="$3"

    echo "  ${url##*/}"
    curl -sSL --fail -o "${out}" "${url}"

    local observed
    observed="$(shasum -a 256 "${out}" | awk '{print $1}')"

    if [[ "${expected_sha}" == "placeholder-fill-in-on-first-run" ]]; then
        echo "    (no pinned SHA yet — observed ${observed})"
        echo "    paste into this script and re-run to lock in"
        return 0
    fi
    if [[ "${observed}" != "${expected_sha}" ]]; then
        echo "  ✗ SHA-256 mismatch for ${url##*/}" >&2
        echo "    expected: ${expected_sha}" >&2
        echo "    observed: ${observed}" >&2
        return 1
    fi
    echo "    sha-256 ok"
}

# --- Download Android arm64 -------------------------------------
ANDROID_XZ="${WORK_DIR}/frida-gadget-android-arm64.so.xz"
download_and_verify \
    "https://github.com/frida/frida/releases/download/${FRIDA_GADGET_VERSION}/frida-gadget-${FRIDA_GADGET_VERSION}-android-arm64.so.xz" \
    "${ANDROID_XZ}" \
    "${ANDROID_ARM64_XZ_SHA256}"

# --- Download iOS universal -------------------------------------
IOS_XZ="${WORK_DIR}/frida-gadget-ios-universal.dylib.xz"
download_and_verify \
    "https://github.com/frida/frida/releases/download/${FRIDA_GADGET_VERSION}/frida-gadget-${FRIDA_GADGET_VERSION}-ios-universal.dylib.xz" \
    "${IOS_XZ}" \
    "${IOS_UNIVERSAL_XZ_SHA256}"

# --- Decompress + install ---------------------------------------
mkdir -p "${GADGET_DIR}/arm64-v8a" "${GADGET_DIR}/ios-universal"

xz -d -k -c "${ANDROID_XZ}" > "${GADGET_DIR}/arm64-v8a/libfrida-gadget.so"
xz -d -k -c "${IOS_XZ}" > "${GADGET_DIR}/ios-universal/FridaGadget.dylib"

# --- Quick sanity check -----------------------------------------
# Verify the decompressed binaries have the right magic — the
# `cargo test` suite catches this too but this gives a fast
# diagnostic before someone commits the result.
android_magic="$(head -c 4 "${GADGET_DIR}/arm64-v8a/libfrida-gadget.so" | od -An -c | tr -d ' \n')"
ios_magic_hex="$(head -c 4 "${GADGET_DIR}/ios-universal/FridaGadget.dylib" | od -An -tx1 | tr -d ' \n')"

if [[ "${android_magic}" != $'\177ELF' ]]; then
    echo "  ✗ android gadget doesn't look like an ELF" >&2
    exit 1
fi
case "${ios_magic_hex}" in
    cafebabe|cafebabf|feedfacf) ;;
    *)
        echo "  ✗ iOS gadget magic ${ios_magic_hex} isn't a Mach-O" >&2
        exit 1
        ;;
esac

echo
echo "Installed:"
ls -la "${GADGET_DIR}/arm64-v8a/" "${GADGET_DIR}/ios-universal/"
echo
echo "If the placeholder SHAs were just filled in, commit this script and"
echo "the binaries together so the version + hashes stay in lockstep."
