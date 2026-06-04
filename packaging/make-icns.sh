#!/usr/bin/env bash
# Generate packaging/Glass.icns from icons/icon.png.
#
# macOS icon bundles are .icns files built by `iconutil` from an
# .iconset directory containing PNGs at canonical sizes:
#
#     icon_16x16.png       icon_16x16@2x.png    (= 32x32)
#     icon_32x32.png       icon_32x32@2x.png    (= 64x64)
#     icon_128x128.png     icon_128x128@2x.png  (= 256x256)
#     icon_256x256.png     icon_256x256@2x.png  (= 512x512)
#     icon_512x512.png     icon_512x512@2x.png  (= 1024x1024)
#
# Each entry name encodes the logical size; the @2x variant is the
# Retina version at twice the pixel count. macOS picks the right
# size for the surface (dock, Finder list, Cmd-Tab switcher).
#
# Run this whenever icons/icon.png changes; the result is checked
# in so the .app build doesn't depend on this script every time.
#
# Pre-requisites: `sips` + `iconutil` (both ship with macOS).
# No ImageMagick required.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SRC="${REPO_ROOT}/icons/icon.png"
OUT="${REPO_ROOT}/packaging/Glass.icns"

if [[ ! -f "${SRC}" ]]; then
    echo "no source icon at ${SRC}" >&2
    exit 1
fi

# Walk through a scratch dir so the script stays idempotent and
# leaves no half-built .iconset behind if iconutil bails.
WORK_DIR="$(mktemp -d -t glass-icns)"
trap 'rm -rf "${WORK_DIR}"' EXIT
ICONSET_DIR="${WORK_DIR}/Glass.iconset"
mkdir -p "${ICONSET_DIR}"

# Step 1: square the source.
# icons/icon.png is 536x520 — slightly taller than wide as drawn.
# macOS icon slots are square, so we pad to the longest dimension
# with the icon's dark background colour (the design already has
# a black frame; an extra 8 black pixels at the bottom land
# invisibly inside it). Better than `sips -z`-resizing direct,
# which would squash by ~3% on one axis.
SRC_W="$(sips -g pixelWidth "${SRC}" | awk '/pixelWidth/ {print $2}')"
SRC_H="$(sips -g pixelHeight "${SRC}" | awk '/pixelHeight/ {print $2}')"
SQUARE=$(( SRC_W > SRC_H ? SRC_W : SRC_H ))

SQUARED="${WORK_DIR}/squared.png"
cp "${SRC}" "${SQUARED}"
if [[ "${SRC_W}" -ne "${SRC_H}" ]]; then
    # sips -p pads with --padColor; default is white, override to
    # black so it blends into the icon's dark background.
    sips -p "${SQUARE}" "${SQUARE}" --padColor 000000 "${SQUARED}" >/dev/null
fi

# Step 2: resample to each canonical size. We always start from
# the squared source rather than chaining downsamples, so each
# output is one resample step (less blur than 1024 -> 512 -> 256
# -> ...).
gen_size() {
    local size="$1" out_name="$2"
    sips -z "${size}" "${size}" "${SQUARED}" \
        --out "${ICONSET_DIR}/${out_name}" >/dev/null
}

gen_size   16 icon_16x16.png
gen_size   32 icon_16x16@2x.png
gen_size   32 icon_32x32.png
gen_size   64 icon_32x32@2x.png
gen_size  128 icon_128x128.png
gen_size  256 icon_128x128@2x.png
gen_size  256 icon_256x256.png
gen_size  512 icon_256x256@2x.png
gen_size  512 icon_512x512.png
gen_size 1024 icon_512x512@2x.png

# Step 3: bundle into .icns. iconutil reads the directory name
# (must end in .iconset) and emits a single binary file.
iconutil --convert icns --output "${OUT}" "${ICONSET_DIR}"

SIZE="$(du -h "${OUT}" | cut -f1)"
echo "wrote ${OUT} (${SIZE})"
