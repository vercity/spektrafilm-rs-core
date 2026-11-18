#!/usr/bin/env bash
set -euo pipefail

ICON_SOURCE="${ICON_SOURCE:-assets/spektrafilm-icon.jpg}"
BUILD_TARGET_DIR="${BUILD_TARGET_DIR:-target/macos-app}"
DIST_DIR="${DIST_DIR:-dist/spektrafilm-macos}"
APP_NAME="${APP_NAME:-Spektrafilm}"
APP_BUNDLE="$DIST_DIR/$APP_NAME.app"

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

if [[ -z "${SPEKTRAFILM_LIBRAW_INCLUDE_DIR:-}" && -z "${SPEKTRAFILM_LIBRAW_LIBRARY_DIR:-}" ]]; then
    if ! command -v brew >/dev/null || ! libraw_prefix="$(brew --prefix libraw)" \
        || [[ ! -f "$libraw_prefix/lib/libraw_r.dylib" ]]; then
        echo "Install LibRaw 0.22 or later for packaging: brew install libraw" >&2
        exit 1
    fi
    export SPEKTRAFILM_LIBRAW_INCLUDE_DIR="$libraw_prefix/include"
    export SPEKTRAFILM_LIBRAW_LIBRARY_DIR="$libraw_prefix/lib"
fi

if [[ -z "${SPEKTRAFILM_EXIV2_INCLUDE_DIR:-}" && -z "${SPEKTRAFILM_EXIV2_LIBRARY_DIR:-}" ]]; then
    if ! command -v brew >/dev/null || ! exiv2_prefix="$(brew --prefix exiv2)" \
        || [[ ! -f "$exiv2_prefix/lib/libexiv2.dylib" ]]; then
        echo "Install Exiv2 0.28 or later for packaging: brew install exiv2" >&2
        exit 1
    fi
    export SPEKTRAFILM_EXIV2_INCLUDE_DIR="$exiv2_prefix/include"
    export SPEKTRAFILM_EXIV2_LIBRARY_DIR="$exiv2_prefix/lib"
fi

# Exiv2 can start with a shorter @rpath install name than its bundled path.
export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C link-arg=-Wl,-headerpad_max_install_names"
cargo build --locked --release --target-dir "$BUILD_TARGET_DIR" \
    -p spektrafilm-gui -p spektrafilm-cli \
    --features spektrafilm-gui/libraw,spektrafilm-gui/exiv2,spektrafilm-cli/libraw,spektrafilm-cli/exiv2 \
    --bin spektrafilm-gui --bin spektrafilm --bin spektrafilm-lut
cargo build --locked --release --target-dir "$BUILD_TARGET_DIR" -p spektrafilm-cli \
    --features libraw,exiv2,precision-f64 --bin spektrafilm-f64

rm -rf "$DIST_DIR"
mkdir -p "$APP_BUNDLE/Contents/MacOS" "$APP_BUNDLE/Contents/Resources/Notices"

for executable in spektrafilm-gui spektrafilm spektrafilm-f64 spektrafilm-lut; do
    cp "$BUILD_TARGET_DIR/release/$executable" "$APP_BUNDLE/Contents/MacOS/"
done
cp -R data "$APP_BUNDLE/Contents/Resources/data"
cp LICENSE SPEKTRAFILM_LICENSE.txt "$APP_BUNDLE/Contents/Resources/Notices/"
bash scripts/bundle_macos_dylibs.sh "$APP_BUNDLE" \
    "$SPEKTRAFILM_LIBRAW_LIBRARY_DIR" "$SPEKTRAFILM_EXIV2_LIBRARY_DIR"

# The selected Homebrew bottle may require a newer macOS than the Rust app.
minimum_macos="$(
    {
        echo 12.0
        for file in "$APP_BUNDLE/Contents/Frameworks/"*.dylib "$APP_BUNDLE/Contents/MacOS/"*; do
            vtool -show-build "$file" \
                | awk '$2 == "LC_VERSION_MIN_MACOSX" { legacy = 1 }
                       $1 == "minos" || (legacy && $1 == "version") { print $2; legacy = 0 }'
        done
    } | sort -t . -k1,1n -k2,2n -k3,3n | tail -n 1
)"
cat > "$APP_BUNDLE/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key>
  <string>en</string>
  <key>CFBundleDisplayName</key>
  <string>Spektrafilm</string>
  <key>CFBundleExecutable</key>
  <string>spektrafilm-gui</string>
  <key>CFBundleIconFile</key>
  <string>spektrafilm.icns</string>
  <key>CFBundleIdentifier</key>
  <string>dev.spektrafilm.app</string>
  <key>CFBundleInfoDictionaryVersion</key>
  <string>6.0</string>
  <key>CFBundleName</key>
  <string>Spektrafilm</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>0.1.2</string>
  <key>CFBundleVersion</key>
  <string>0.1.2</string>
  <key>LSMinimumSystemVersion</key>
  <string>$minimum_macos</string>
  <key>NSHighResolutionCapable</key>
  <true/>
</dict>
</plist>
PLIST

if [[ -f "$ICON_SOURCE" ]]; then
    iconset="$BUILD_TARGET_DIR/spektrafilm.iconset"
    rm -rf "$iconset"
    mkdir -p "$iconset"
    sips -s format png -z 16 16 "$ICON_SOURCE" --out "$iconset/icon_16x16.png" >/dev/null
    sips -s format png -z 32 32 "$ICON_SOURCE" --out "$iconset/icon_16x16@2x.png" >/dev/null
    sips -s format png -z 32 32 "$ICON_SOURCE" --out "$iconset/icon_32x32.png" >/dev/null
    sips -s format png -z 64 64 "$ICON_SOURCE" --out "$iconset/icon_32x32@2x.png" >/dev/null
    sips -s format png -z 128 128 "$ICON_SOURCE" --out "$iconset/icon_128x128.png" >/dev/null
    sips -s format png -z 256 256 "$ICON_SOURCE" --out "$iconset/icon_128x128@2x.png" >/dev/null
    sips -s format png -z 256 256 "$ICON_SOURCE" --out "$iconset/icon_256x256.png" >/dev/null
    sips -s format png -z 512 512 "$ICON_SOURCE" --out "$iconset/icon_256x256@2x.png" >/dev/null
    sips -s format png -z 512 512 "$ICON_SOURCE" --out "$iconset/icon_512x512.png" >/dev/null
    sips -s format png -z 1024 1024 "$ICON_SOURCE" --out "$iconset/icon_512x512@2x.png" >/dev/null
    iconutil -c icns "$iconset" -o "$APP_BUNDLE/Contents/Resources/spektrafilm.icns"
fi

cat > "$DIST_DIR/README.txt" <<'README'
spektrafilm-rs macOS

Run Spektrafilm.app for the GUI.

Backends:
- macOS uses the WGSL/wgpu backend through Metal by default.
- CUDA is not available on modern macOS builds.

RAW development uses the bundled LibRaw; no separate install is required.
JPEG, PNG, and TIFF exports preserve camera MakerNote data using the bundled Exiv2.

Command-line tools in Spektrafilm.app/Contents/MacOS:
- spektrafilm: image processing (f32) and single 3D CUBE LUT export.
- spektrafilm-f64: image processing (f64), also used by the GUI's CPU f64 Export.
- spektrafilm-lut: LUT bundle export using the Python Spektrafilm command syntax.

License and dependency notices are in Spektrafilm.app/Contents/Resources/Notices.
This software is based in part on the work of the Independent JPEG Group.

Gatekeeper:
- This package is ad-hoc signed for local testing. It is not Apple-notarized.
- For opening an app downloaded from a source you trust, see:
  https://support.apple.com/en-us/102445
README

for file in "$APP_BUNDLE/Contents/Frameworks/"*.dylib "$APP_BUNDLE/Contents/MacOS/"*; do
    codesign --force --sign - "$file"
done
codesign --force --sign - "$APP_BUNDLE"
codesign --verify --deep --strict "$APP_BUNDLE"

zip_path="$DIST_DIR.zip"
rm -f "$zip_path"
(
    cd "$DIST_DIR/.."
    ditto -c -k --sequesterRsrc --keepParent "$(basename "$DIST_DIR")" "$(basename "$zip_path")"
)

echo "Packaged: $(realpath "$DIST_DIR")"
echo "Zip:      $(realpath "$zip_path")"
