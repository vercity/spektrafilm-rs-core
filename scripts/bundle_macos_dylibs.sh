#!/usr/bin/env bash
# Copy the linked dylibs and rewrite install names before signing the app.
set -euo pipefail

app="$1"
library_dir="$2"
frameworks="$app/Contents/Frameworks"
notices="$app/Contents/Resources/Notices"
mkdir -p "$frameworks" "$notices"

shopt -s nullglob
copy_notices() {
    local source="$1" cellar relative keg notice destination
    if [[ -n "${SPEKTRAFILM_MACOS_NOTICES_DIR:-}" ]]; then
        # For a custom LibRaw installation, the caller supplies all runtime notices.
        return
    fi

    cellar="$(realpath "$(brew --cellar)")"
    relative="${source#"$cellar/"}"
    if [[ "$relative" == "$source" ]]; then
        echo "Provide SPEKTRAFILM_MACOS_NOTICES_DIR for the custom dylib: $source" >&2
        return 1
    fi

    # A real path in a Homebrew keg has the layout <formula>/<version>/lib/...
    keg="${relative%/lib/*}"
    destination="$notices/$keg"
    local files=("$cellar/$keg"/LICENSE* "$cellar/$keg"/COPYING* "$cellar/$keg"/COPYRIGHT*
                 "$cellar/$keg"/NOTICE* "$cellar/$keg"/README.ijg
                 "$cellar/$keg"/share/doc/*/README.ijg)
    if (( ${#files[@]} == 0 )); then
        echo "No notices found in the Homebrew keg: $cellar/$keg" >&2
        return 1
    fi
    mkdir -p "$destination"
    for notice in "${files[@]}"; do
        cp -RL "$notice" "$destination/"
    done
}

# Canonical source paths keep basename collisions from silently binding the
# package to the wrong library. Use indexed arrays for macOS's Bash 3.2.
names=()
sources=()
relocate() {
    local original="$1" target="$2" loader="$3"
    local dylib_id="" deps dep name source bundled i
    if [[ "$target" == "$frameworks/"* ]]; then
        dylib_id="$(otool -DX "$original")"
    fi
    deps="$(otool -LX "$original" | sed -E 's/^[[:space:]]+//; s/ \(compatibility version .*$//')"
    while IFS= read -r dep; do
        case "$dep" in
            ""|"$dylib_id"|/usr/lib/*|/System/Library/*) continue ;;
            @loader_path/*) source="${original%/*}/${dep#@loader_path/}" ;;
            @rpath/*) source="$library_dir/${dep#@rpath/}" ;;
            /*) source="$dep" ;;
            *) source="" ;;
        esac
        name="${dep##*/}"
        if [[ ! -f "$source" ]]; then
            # Custom distributions can retain an absolute install name from
            # their build machine even though the dylib itself has moved.
            source="$library_dir/$name"
        fi
        if [[ ! -f "$source" ]]; then
            echo "Cannot locate $dep, required by $original" >&2
            return 1
        fi
        source="$(realpath "$source")"
        bundled="$frameworks/$name"
        for ((i = 0; i < ${#names[@]}; i++)); do
            if [[ "${names[$i]}" == "$name" && "${sources[$i]}" != "$source" ]]; then
                echo "Dylib basename collision for $name: ${sources[$i]} and $source" >&2
                return 1
            fi
        done
        if [[ ! -f "$bundled" ]]; then
            names+=("$name")
            sources+=("$source")
            cp "$source" "$bundled"
            chmod u+w "$bundled"
            copy_notices "$source"
            install_name_tool -id "@loader_path/$name" "$bundled"
            relocate "$source" "$bundled" '@loader_path'
        fi
        if [[ "$dep" != "$loader/$name" ]]; then
            install_name_tool -change "$dep" "$loader/$name" "$target"
        fi
    done <<< "$deps"
}

if [[ -n "${SPEKTRAFILM_MACOS_NOTICES_DIR:-}" ]]; then
    cp -RL "$SPEKTRAFILM_MACOS_NOTICES_DIR/." "$notices/"
fi
for executable in "$app/Contents/MacOS/"*; do
    relocate "$executable" "$executable" '@loader_path/../Frameworks'
    # LUT generation has no RAW input and may omit LibRaw when linking.
    if [[ "${executable##*/}" != spektrafilm-lut ]] \
        && ! otool -LX "$executable" | grep -q '@loader_path/../Frameworks/libraw'; then
        echo "Bundled LibRaw dependency missing from $executable" >&2
        exit 1
    fi
done
