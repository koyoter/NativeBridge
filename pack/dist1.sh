#!/usr/bin/env bash
# dist1.sh — assemble the NativeBridge Unity package from the per-platform
# install_* artifacts produced by build.ps1.
#
# Usage (from repo root, after all platform build artifacts are present):
#   bash pack/dist1.sh <version>
#
# Produces:
#   NativeBridge/                     (Unity package tree, see below)
#   NativeBridge-CSharpe-<version>.zip
#
# Expected inputs (any missing platform is skipped with a warning):
#   install_win32_x64/nativebridge/bin/NativeBridge.dll
#   install_win32_arm64/nativebridge/bin/NativeBridge.dll
#   install_linux_x64/nativebridge/lib/libNativeBridge.so
#   install_linux_arm64/nativebridge/lib/libNativeBridge.so
#   install_android_*/nativebridge/lib/libNativeBridge.so
#   install_osx_x64|arm64/nativebridge/lib/libNativeBridge.dylib
#   install_ios_arm64/.../libNativeBridge.a  + sim + tvos  (xcframework)
set -u

VERSION="${1:-1.0.0}"
LIB=nativebridge
PKG=NativeBridge
OUT="$PKG"
ZIP="${PKG}-${VERSION}.zip"

echo "NativeBridge dist: version=$VERSION"

rm -rf "$OUT"
mkdir -p "$OUT/Runtime" "$OUT/Plugins"

# --- C# runtime sources -----------------------------------------------------
cp -f csharp/NativeBridge/Curlw.cs "$OUT/Runtime/" 2>/dev/null || echo "WARN: Curlw.cs missing"
cp -f csharp/NativeBridge/Vfs.cs "$OUT/Runtime/" 2>/dev/null || echo "WARN: Vfs.cs missing"
cp -f csharp/NativeBridge/DownloadManager.cs "$OUT/Runtime/" 2>/dev/null || echo "WARN: DownloadManager.cs missing"
cp -f csharp/NativeBridge/NativeBridgeF.asmdef "$OUT/Runtime/" 2>/dev/null || echo "WARN: NativeBridgeF.asmdef missing"

# --- helper: copy a file if it exists, else warn ---------------------------
copy_if() { # src dstdir
    if [ -f "$1" ]; then
        mkdir -p "$2"
        cp -f "$1" "$2/"
        echo "  + $1 -> $2"
    else
        echo "  - skip (missing): $1"
    fi
}

# --- Windows ----------------------------------------------------------------
copy_if "install_win32_x64/$LIB/bin/NativeBridge.dll"   "$OUT/Plugins/Windows/x86_64"
copy_if "install_win32_arm64/$LIB/bin/NativeBridge.dll" "$OUT/Plugins/Windows/ARM64"

# --- Linux ------------------------------------------------------------------
copy_if "install_linux_x64/$LIB/lib/libNativeBridge.so"   "$OUT/Plugins/Linux/x86_64"
copy_if "install_linux_arm64/$LIB/lib/libNativeBridge.so" "$OUT/Plugins/Linux/ARM64"

# --- Android ----------------------------------------------------------------
copy_if "install_android_arm64/$LIB/lib/libNativeBridge.so" "$OUT/Plugins/Android/arm64-v8a"
copy_if "install_android_armv7/$LIB/lib/libNativeBridge.so" "$OUT/Plugins/Android/armeabi-v7a"
copy_if "install_android_x64/$LIB/lib/libNativeBridge.so"   "$OUT/Plugins/Android/x86_64"
copy_if "install_android_x86/$LIB/lib/libNativeBridge.so"   "$OUT/Plugins/Android/x86"

# --- macOS (universal .bundle-less dylib) -----------------------------------
MAC_X64="install_osx_x64/$LIB/lib/libNativeBridge.dylib"
MAC_ARM="install_osx_arm64/$LIB/lib/libNativeBridge.dylib"
if [ -f "$MAC_X64" ] && [ -f "$MAC_ARM" ] && command -v lipo >/dev/null 2>&1; then
    mkdir -p "$OUT/Plugins/macOS"
    lipo -create "$MAC_X64" "$MAC_ARM" -output "$OUT/Plugins/macOS/NativeBridge.dylib"
    lipo -info "$OUT/Plugins/macOS/NativeBridge.dylib"
    echo "  + macOS universal dylib"
else
    copy_if "$MAC_ARM" "$OUT/Plugins/macOS"
    copy_if "$MAC_X64" "$OUT/Plugins/macOS"
fi

# --- iOS / tvOS xcframeworks (static libs) ----------------------------------
# Build ONE xcframework per Unity platform folder: Unity only searches
# Plugins/iOS for iOS builds and Plugins/tvOS for tvOS builds, so an xcframework
# containing tvOS slices must not live under Plugins/iOS (Unity tvOS wouldn't
# find it). Device + simulator slices are combined within each platform's
# xcframework; simulator arm64+x64 are lipo'd into one fat archive first.
build_xcframework() {
    if ! command -v xcodebuild >/dev/null 2>&1; then
        echo "  - skip xcframework (xcodebuild unavailable)"
        return
    fi

    local plat="$1"       # ios | tvos
    local out_subdir="$2" # iOS | tvOS
    local dev="install_${plat}_arm64/$LIB/lib/libNativeBridge.a"
    local sim_arm="install_${plat}_arm64_sim/$LIB/lib/libNativeBridge.a"
    local sim_x64="install_${plat}_x64/$LIB/lib/libNativeBridge.a"

    local args=()
    local tmp="fat_tmp_${LIB}_${plat}"
    rm -rf "$tmp"; mkdir -p "$tmp"

    # device slice
    [ -f "$dev" ] && args+=(-library "$dev")

    # simulator slice (arm64 + x64 -> fat)
    if [ -f "$sim_arm" ] || [ -f "$sim_x64" ]; then
        local inputs=(); [ -f "$sim_arm" ] && inputs+=("$sim_arm"); [ -f "$sim_x64" ] && inputs+=("$sim_x64")
        lipo -create "${inputs[@]}" -output "$tmp/${plat}_sim.a"
        args+=(-library "$tmp/${plat}_sim.a")
    fi

    if [ ${#args[@]} -eq 0 ]; then
        echo "  - skip ${plat} xcframework (no static libs found)"
        rm -rf "$tmp"
        return
    fi

    mkdir -p "$OUT/Plugins/${out_subdir}"
    xcodebuild -create-xcframework "${args[@]}" -output "$OUT/Plugins/${out_subdir}/NativeBridge.xcframework"
    echo "  + Plugins/${out_subdir}/NativeBridge.xcframework"
    rm -rf "$tmp"
}
build_xcframework ios  iOS
build_xcframework tvos tvOS

# --- version manifest -------------------------------------------------------
{
    echo "nativebridge: $VERSION"
    echo "curlw_abi: 1"
    for verf in install_*/curl/_1kiss; do
        [ -f "$verf" ] && echo "curl: $(head -1 "$verf" | sed 's/^ver:[[:space:]]*//')" && break
    done
} > "$OUT/_nativebridge.yml"

# --- UPM package manifest ---------------------------------------------------
# Makes the folder installable via Unity Package Manager ("Add package from
# disk") in addition to being a drop-in Assets/ package. Layout (Runtime/ +
# Plugins/) is already UPM-conformant.
cat > "$OUT/package.json" <<EOF
{
  "name": "com.nativebridge.unity",
  "displayName": "NativeBridge",
  "version": "$VERSION",
  "unity": "2021.3",
  "description": "Self-contained Unity native library: curl HTTP/1.1+2+3 (curl + BoringSSL + nghttp2/nghttp3/ngtcp2 + zlib) with C# bindings.",
  "author": { "name": "Koyoter" }
}
EOF

# --- Unity .meta files ------------------------------------------------------
# Ship a .meta for every folder/file with a DETERMINISTIC guid (md5 of the
# package-relative path): re-running dist or updating the package keeps GUIDs
# stable, so Unity has nothing to regenerate and references survive upgrades.
# Native binaries get explicit PluginImporter platform settings instead of
# Unity's import-time auto-detection. .xcframework bundles are single plugin
# assets for Unity; their contents are not traversed.
meta_guid() { # package-relative path -> 32 hex chars
    if command -v md5sum >/dev/null 2>&1; then
        printf '%s' "$1" | md5sum | cut -d' ' -f1
    else
        printf '%s' "$1" | md5 -q
    fi
}

write_meta() { # dst_path guid importer_yaml_body
    printf 'fileFormatVersion: 2\nguid: %s\n%s\n' "$2" "$3" > "$1"
}

META_FOLDER='folderAsset: yes
DefaultImporter:
  externalObjects: {}
  userData: 
  assetBundleName: 
  assetBundleVariant:'

META_MONO='MonoImporter:
  externalObjects: {}
  serializedVersion: 2
  defaultReferences: []
  executionOrder: 0
  icon: {instanceID: 0}
  userData: 
  assetBundleName: 
  assetBundleVariant:'

META_ASMDEF='AssemblyDefinitionImporter:
  externalObjects: {}
  userData: 
  assetBundleName: 
  assetBundleVariant:'

META_TEXT='TextScriptImporter:
  externalObjects: {}
  userData: 
  assetBundleName: 
  assetBundleVariant:'

plugin_meta() { # dst guid spec...  spec = first-key|first-value|cpu (cpu may be empty)
    # Unity parses each `first` map as platform-name -> platform-name pairs
    # (`Editor: Editor`, `Standalone: Win64`); `Any:` is the lone empty-value
    # entry. Names and CPU values must be Unity's exact serialized form — one
    # unrecognized entry and the deserializer drops the WHOLE platformData and
    # resets the importer to Any Platform (the v0.1.0 import error). Platforms
    # not listed stay disabled; keep the Any:0 entry or they all turn on.
    local dst="$1" guid="$2"; shift 2
    {
        printf 'fileFormatVersion: 2\nguid: %s\n' "$guid"
        printf 'PluginImporter:\n'
        printf '  externalObjects: {}\n  serializedVersion: 2\n'
        printf '  iconMap: {}\n  executionOrder: {}\n'
        printf '  defineConstraints: []\n'
        printf '  isPreloaded: 0\n  isOverridable: 0\n  isExplicitlyReferenced: 0\n  validateReferences: 1\n'
        printf '  platformData:\n'
        printf '  - first:\n      Any: \n    second:\n      enabled: 0\n      settings: {}\n'
        local spec key rest val cpu
        for spec in "$@"; do
            key="${spec%%|*}"
            rest="${spec#*|}"
            val="${rest%%|*}"
            cpu="${rest#*|}"
            [ "$cpu" = "$rest" ] && cpu=''
            printf '  - first:\n      %s: %s\n    second:\n      enabled: 1\n' "$key" "$val"
            if [ -n "$cpu" ]; then
                printf '      settings:\n        CPU: %s\n' "$cpu"
            else
                printf '      settings: {}\n'
            fi
        done
        printf '  userData: \n  assetBundleName: \n  assetBundleVariant: \n'
    } > "$dst"
}

gen_meta() { # absolute path under $OUT
    local p="$1" rel guid
    rel="${p#"$OUT"/}"
    guid=$(meta_guid "$rel")
    # .xcframework dirs are plugin assets, NOT folders: dispatch before the
    # directory check or they'd get a folderAsset meta and Unity would try to
    # import their contents individually.
    case "$rel" in
        Plugins/iOS/*.xcframework)  plugin_meta "$p.meta" "$guid" 'iPhone|iPhone|'; return ;;
        Plugins/tvOS/*.xcframework) plugin_meta "$p.meta" "$guid" 'tvOS|tvOS|'; return ;;
    esac
    if [ -d "$p" ]; then
        write_meta "$p.meta" "$guid" "$META_FOLDER"
        return
    fi
    case "$rel" in
        *.cs)         write_meta "$p.meta" "$guid" "$META_MONO" ;;
        *.asmdef)     write_meta "$p.meta" "$guid" "$META_ASMDEF" ;;
        *.json|*.yml) write_meta "$p.meta" "$guid" "$META_TEXT" ;;
        *)
            # Editor entries: only where the binary can actually load in an
            # editor (x64 Windows/Linux editors, macOS universal). Absent
            # platforms stay disabled via the Any:0 entry.
            case "$rel" in
                Plugins/Windows/x86_64/*)     plugin_meta "$p.meta" "$guid" 'Editor|Editor|x86_64' 'Standalone|Win64|x86_64' ;;
                Plugins/Windows/ARM64/*)      plugin_meta "$p.meta" "$guid" 'Standalone|Win64|ARM64' ;;
                Plugins/Linux/x86_64/*)       plugin_meta "$p.meta" "$guid" 'Editor|Editor|x86_64' 'Standalone|Linux64|x86_64' ;;
                Plugins/Linux/ARM64/*)        plugin_meta "$p.meta" "$guid" 'Standalone|Linux64|ARM64' ;;
                Plugins/Android/*)            plugin_meta "$p.meta" "$guid" 'Android|Android|' ;;
                Plugins/macOS/*)              plugin_meta "$p.meta" "$guid" 'Editor|Editor|AnyCPU' 'Standalone|OSXUniversal|AnyCPU' ;;
                Plugins/iOS/*)                plugin_meta "$p.meta" "$guid" 'iPhone|iPhone|' ;;
                Plugins/tvOS/*)               plugin_meta "$p.meta" "$guid" 'tvOS|tvOS|' ;;
                *)
                    echo "WARN: no Unity importer template for '$rel'; meta not written"
                    ;;
            esac
            ;;
    esac
}

while IFS= read -r -d '' p; do
    gen_meta "$p"
done < <(find "$OUT" -mindepth 1 \( -name '*.xcframework' -prune -print0 \) -o -print0)

# --- zip --------------------------------------------------------------------
rm -f "$ZIP"
if command -v zip >/dev/null 2>&1; then
    zip -q -r "$ZIP" "$OUT"
    echo "NativeBridge dist: wrote $ZIP"
else
    echo "WARN: zip not found; package tree left at $OUT/"
fi

# Export for the GitHub release step.
if [ -n "${GITHUB_ENV:-}" ]; then
    echo "NB_DIST_ZIP=$ZIP" >> "$GITHUB_ENV"
    echo "NB_DIST_DIR=$OUT" >> "$GITHUB_ENV"
fi
