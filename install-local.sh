#!/bin/sh
# Build the current local source and install Oxru:
#   1. the `oxru` CLI into ~/.cargo/bin (so you can run it from anywhere), and
#   2. on macOS, the "Oxru.app" bundle (Launchpad / Spotlight / double-click).
#
#     ./install-local.sh
#
# Run it whenever you want to update to the latest local changes.
# (For installing the published version from GitHub instead, use install.sh.)
set -eu

# Always operate from the repo root, wherever this script is invoked from.
cd "$(dirname "$0")"

green() { printf '\033[0;32m%s\033[0m\n' "$1"; }
blue()  { printf '\033[0;34m%s\033[0m\n' "$1"; }
red()   { printf '\033[0;31m%s\033[0m\n' "$1"; }

if ! command -v cargo >/dev/null 2>&1; then
    red "Need 'cargo' — install Rust from https://rustup.rs"
    exit 1
fi

# Stop any running copies so the binary isn't "text file busy" during install.
# LC_ALL=C is REQUIRED: under a UTF-8 locale macOS `pkill -f` aborts with
# "illegal byte sequence" the moment it scans a process whose argv has a
# non-UTF-8 byte — killing nothing and (thanks to `|| true`) failing silently,
# which is why a running Oxru window used to survive a reinstall. C locale
# matches the argv bytes raw, so the kill actually works.
LC_ALL=C pkill -f '/oxru( |$)' 2>/dev/null || true
LC_ALL=C pkill -f 'Oxru.app' 2>/dev/null || true

# ---- build once ------------------------------------------------------------
blue "Building release binary…"
cargo build --release >/dev/null
BIN="target/release/oxru"

# ---- 1. install the CLI ----------------------------------------------------
BIN_DIR="${CARGO_HOME:-$HOME/.cargo}/bin"
mkdir -p "$BIN_DIR"
cp "$BIN" "$BIN_DIR/oxru"
green "Installed CLI -> $BIN_DIR/oxru"

case ":${PATH}:" in
    *":${BIN_DIR}:"*) : ;;
    *)
        echo "Note: ${BIN_DIR} isn't on your PATH. Add to your shell profile:"
        echo "    export PATH=\"${BIN_DIR}:\$PATH\""
        ;;
esac

# ---- 2. macOS app bundle ---------------------------------------------------
if [ "$(uname -s)" = "Darwin" ]; then
    if [ -w /Applications ]; then
        DEST="/Applications/Oxru.app"
    else
        mkdir -p "$HOME/Applications"
        DEST="$HOME/Applications/Oxru.app"
    fi

    blue "Assembling $DEST"
    rm -rf "$DEST"
    mkdir -p "$DEST/Contents/MacOS"

    # The real binary, bundled so the process is associated with the .app. Named
    # distinctly from the launcher ("Oxru") because macOS filesystems are usually
    # case-insensitive — "Oxru" and "oxru" would otherwise collide.
    cp "$BIN" "$DEST/Contents/MacOS/oxru-bin"

    # Launcher: open Oxru with NO folder (the welcome screen) — the user picks a
    # folder from the recents picker (Ctrl+O).
    cat > "$DEST/Contents/MacOS/Oxru" <<'LAUNCH'
#!/bin/sh
HERE="$(cd "$(dirname "$0")" && pwd)"
exec "$HERE/oxru-bin" --gui
LAUNCH
    chmod +x "$DEST/Contents/MacOS/Oxru" "$DEST/Contents/MacOS/oxru-bin"

    cat > "$DEST/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>            <string>Oxru</string>
    <key>CFBundleDisplayName</key>     <string>Oxru</string>
    <key>CFBundleExecutable</key>      <string>Oxru</string>
    <key>CFBundleIconFile</key>        <string>AppIcon</string>
    <key>CFBundleIdentifier</key>      <string>com.p32929.oxru</string>
    <key>CFBundlePackageType</key>     <string>APPL</string>
    <key>CFBundleVersion</key>         <string>0.1.0</string>
    <key>CFBundleShortVersionString</key> <string>0.1.0</string>
    <key>LSMinimumSystemVersion</key>  <string>10.13</string>
    <key>NSHighResolutionCapable</key> <true/>
    <!-- Keep running normally when backgrounded: App Nap otherwise throttles the
         window, freezing embedded terminal output until it's focused again. -->
    <key>NSAppSleepDisabled</key>      <true/>
    <key>LSApplicationCategoryType</key> <string>public.app-category.developer-tools</string>
    <!-- Reason strings shown in the macOS file-access prompts. Required so the
         system shows a clear prompt (and grants on Allow) rather than silently
         denying access to these protected folders. -->
    <key>NSDocumentsFolderUsageDescription</key> <string>Oxru opens code projects stored in your Documents folder.</string>
    <key>NSDesktopFolderUsageDescription</key>   <string>Oxru opens code projects stored on your Desktop.</string>
    <key>NSDownloadsFolderUsageDescription</key> <string>Oxru opens code projects stored in your Downloads folder.</string>
</dict>
</plist>
PLIST

    # App icon: build Resources/AppIcon.icns from assets/icon.png at the standard
    # iconset sizes. Best-effort — a missing source or tool just leaves it iconless.
    ICON_SRC="assets/icon.png"
    if [ -f "$ICON_SRC" ] && command -v sips >/dev/null 2>&1 && command -v iconutil >/dev/null 2>&1; then
        mkdir -p "$DEST/Contents/Resources"
        TMPSET="$(mktemp -d)"
        ICONSET="$TMPSET/AppIcon.iconset"
        mkdir -p "$ICONSET"

        # `iconutil` rejects opaque (alpha-less) source images with "Failed to
        # generate ICNS" on current macOS — assets/icon.png has no alpha
        # channel, and `sips` has no way to add one on its own (its
        # --setProperty doesn't cover hasAlpha). Draw it into a real RGBA
        # bitmap once via a tiny Swift/CoreGraphics helper (Swift ships with
        # the Xcode Command Line Tools this project's own build already
        # requires) so every resized copy below carries a proper alpha
        # channel; silently fall back to the original source if Swift isn't
        # available or the conversion fails for some other reason.
        RGBA_SRC="$ICON_SRC"
        if command -v swift >/dev/null 2>&1; then
            ALPHA_HELPER="$TMPSET/add_alpha.swift"
            cat > "$ALPHA_HELPER" <<'SWIFT'
import Foundation
import CoreGraphics
import ImageIO
import UniformTypeIdentifiers

let args = CommandLine.arguments
guard args.count == 3,
      let src = CGImageSourceCreateWithURL(URL(fileURLWithPath: args[1]) as CFURL, nil),
      let cgImage = CGImageSourceCreateImageAtIndex(src, 0, nil) else { exit(1) }
let w = cgImage.width, h = cgImage.height
guard let ctx = CGContext(
    data: nil, width: w, height: h, bitsPerComponent: 8, bytesPerRow: 0,
    space: CGColorSpaceCreateDeviceRGB(), bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
) else { exit(1) }
ctx.draw(cgImage, in: CGRect(x: 0, y: 0, width: w, height: h))
guard let outImage = ctx.makeImage(),
      let dest = CGImageDestinationCreateWithURL(URL(fileURLWithPath: args[2]) as CFURL, UTType.png.identifier as CFString, 1, nil)
else { exit(1) }
CGImageDestinationAddImage(dest, outImage, nil)
guard CGImageDestinationFinalize(dest) else { exit(1) }
SWIFT
            if swift "$ALPHA_HELPER" "$ICON_SRC" "$TMPSET/icon_rgba.png" >/dev/null 2>&1; then
                RGBA_SRC="$TMPSET/icon_rgba.png"
            fi
        fi

        for s in 16 32 128 256 512; do
            d=$((s * 2))
            sips -z "$s" "$s" "$RGBA_SRC" --out "$ICONSET/icon_${s}x${s}.png"     >/dev/null 2>&1
            sips -z "$d" "$d" "$RGBA_SRC" --out "$ICONSET/icon_${s}x${s}@2x.png" >/dev/null 2>&1
        done
        if iconutil -c icns "$ICONSET" -o "$DEST/Contents/Resources/AppIcon.icns" >/dev/null 2>&1; then
            green "Installed app icon"
        else
            echo "Note: app icon generation failed — the app will run without one."
        fi
        rm -rf "$TMPSET"
    fi

    # Sign with a STABLE identity if one is available, falling back to ad-hoc.
    #
    # Why this matters: macOS records file-access permission grants ("Allow") and
    # other TCC consent against the app's code-signing identity. An ad-hoc
    # signature has no stable identity — its hash changes on every rebuild — so
    # macOS treats each build as a brand-new app and re-prompts (e.g. "Oxru would
    # like to access your Documents folder") every time you reinstall. Signing
    # with a real identity (Developer ID, or the free Apple Development cert that
    # comes with Xcode) gives a stable designated requirement, so you click Allow
    # once and it sticks across rebuilds — the way VS Code behaves.
    SIGN_ID="$(security find-identity -v -p codesigning 2>/dev/null \
        | awk -F'"' '/Developer ID Application/ {print $2; exit}')"
    [ -n "$SIGN_ID" ] || SIGN_ID="$(security find-identity -v -p codesigning 2>/dev/null \
        | awk -F'"' '/Apple Development/ {print $2; exit}')"
    if [ -n "$SIGN_ID" ] && codesign --force --sign "$SIGN_ID" "$DEST" >/dev/null 2>&1; then
        green "Signed with stable identity: $SIGN_ID"
        green "macOS will remember file-access permissions across rebuilds."
    else
        codesign --force --sign - "$DEST" >/dev/null 2>&1 || true
        echo "Note: signed ad-hoc (no stable identity found). macOS will re-ask for"
        echo "      file permissions after each rebuild. To stop that, create a free"
        echo "      'Apple Development' certificate in Xcode (Settings ▸ Accounts) and"
        echo "      re-run this script."
    fi
    # Nudge Launch Services so it appears in Spotlight/Launchpad right away.
    /System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister \
        -f "$DEST" >/dev/null 2>&1 || true

    green "Installed app -> $DEST"
fi

echo
green "Done — Oxru is up to date."
echo "Run it:  oxru --gui ."
[ "$(uname -s)" = "Darwin" ] && echo "Or open it from Launchpad / Spotlight as \"Oxru\"."
exit 0
