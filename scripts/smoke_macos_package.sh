#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
archive="$(realpath "${1:-$repo_root/dist/spektrafilm-macos.zip}")"
unpacked="$(mktemp -d "${TMPDIR:-/tmp}/spektrafilm-macos-smoke.XXXXXX")"
trap 'rm -rf "$unpacked"' EXIT
ditto -x -k "$archive" "$unpacked/Extracted App"
app="$unpacked/Extracted App/$(basename "${archive%.zip}")/Spektrafilm.app"
codesign --verify --deep --strict "$app"

bin="$app/Contents/MacOS"
mkdir "$unpacked/work"
cd "$unpacked/work"
python3 "$repo_root/scripts/make_release_smoke_dng.py" input.dng
unset DYLD_LIBRARY_PATH DYLD_FALLBACK_LIBRARY_PATH DYLD_FRAMEWORK_PATH \
    DYLD_FALLBACK_FRAMEWORK_PATH DYLD_INSERT_LIBRARIES \
    SPEKTRAFILM_DATA_DIR SPEKTRAFILM_F64_CLI
export SPEKTRAFILM_BACKEND=cpu RUST_LOG=spektrafilm_io=debug

"$bin/spektrafilm" list-profiles > profiles.txt
grep -q 'kodak_portra_400' profiles.txt
for exe in spektrafilm spektrafilm-f64; do
    "$bin/$exe" process input.dng --film kodak_portra_400 --scan-film --bit-depth 32 \
        --output "$exe.tiff" \
        2>&1 | tee "$exe.log"
    grep -q 'developed RAW image with LibRaw' "$exe.log"
    dimensions="$(sips -g pixelWidth -g pixelHeight "$exe.tiff")"
    grep -q 'pixelWidth: 96$' <<< "$dimensions"
    grep -q 'pixelHeight: 72$' <<< "$dimensions"
done
"$bin/spektrafilm-lut" build --film kodak_portra_400 --print fujifilm_crystal_archive_typeii \
    --input srgb --output srgb --resolution 2 --out bundles
cube="$(find bundles -name '*.cube' -type f -print | head -n 1)"
test -n "$cube"
grep -q '^LUT_3D_SIZE 2$' "$cube"
