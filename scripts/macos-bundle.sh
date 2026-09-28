#!/bin/sh
#
# Wrap a built `zuno` binary into `Zuno.app`, ad-hoc sign it, and pack it for release.
#
#   cargo build --release -p zuno
#   scripts/macos-bundle.sh target/release/zuno 0.3.0 target/macos
#
# Produces `<out>/Zuno.app` and `<out>/zuno-<version>-aarch64-macos.tar.gz`, the archive
# `install.sh` unpacks into /Applications. macOS only: it needs `codesign`, and `iconutil` for
# the icon.
#
# **Ad-hoc signed, not Developer ID.** Apple Silicon refuses to run an unsigned arm64 binary at
# all, and `--sign -` is the signature that needs no Apple account. It does not satisfy
# Gatekeeper for a *quarantined* download — a browser's — which is why the advertised install is
# `curl | sh`: curl sets no quarantine flag, so there is nothing for Gatekeeper to check.
#
# **The icon is drawn from `assets/icons/zuno.svg`** when `rsvg-convert` is present (`brew install
# librsvg`), at every size an `.icns` holds. Without it the bundle gets no icon and says so;
# `release.yml` asserts the icon exists, so only a local build can ship without one.
#
set -eu

binary=${1:?usage: macos-bundle.sh <binary> <version> <out-dir>}
version=${2:?usage: macos-bundle.sh <binary> <version> <out-dir>}
out=${3:?usage: macos-bundle.sh <binary> <version> <out-dir>}

root=$(cd "$(dirname "$0")/.." && pwd)
app="$out/Zuno.app"

[ -x "$binary" ] || { echo "error: $binary is not an executable" >&2; exit 1; }

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$binary" "$app/Contents/MacOS/zuno"
chmod 755 "$app/Contents/MacOS/zuno"

icon_key=""
if command -v rsvg-convert >/dev/null 2>&1; then
    iconset="$out/zuno.iconset"
    rm -rf "$iconset"
    mkdir -p "$iconset"
    for size in 16 32 128 256 512; do
        rsvg-convert -w "$size" -h "$size" "$root/assets/icons/zuno.svg" \
            -o "$iconset/icon_${size}x${size}.png"
        double=$((size * 2))
        rsvg-convert -w "$double" -h "$double" "$root/assets/icons/zuno.svg" \
            -o "$iconset/icon_${size}x${size}@2x.png"
    done
    iconutil -c icns "$iconset" -o "$app/Contents/Resources/zuno.icns"
    rm -rf "$iconset"
    icon_key="	<key>CFBundleIconFile</key>
	<string>zuno</string>"
else
    echo "warning: rsvg-convert not found (brew install librsvg) — the bundle has no icon" >&2
fi

# `CFBundleIdentifier` is the app_id `main.rs` sets on Linux, so the one identity holds on every
# platform. 11.0 is the first macOS that ran on Apple Silicon, and Rust's own deployment target
# for `aarch64-apple-darwin`, so it claims nothing the binary does not already require.
cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleExecutable</key>
	<string>zuno</string>
	<key>CFBundleIdentifier</key>
	<string>dev.zuno.Zuno</string>
	<key>CFBundleName</key>
	<string>Zuno</string>
	<key>CFBundleDisplayName</key>
	<string>Zuno</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleShortVersionString</key>
	<string>$version</string>
	<key>CFBundleVersion</key>
	<string>$version</string>
$icon_key
	<key>LSMinimumSystemVersion</key>
	<string>11.0</string>
	<key>LSApplicationCategoryType</key>
	<string>public.app-category.developer-tools</string>
	<key>NSHighResolutionCapable</key>
	<true/>
</dict>
</plist>
EOF
plutil -lint "$app/Contents/Info.plist" >/dev/null

# Signed *after* everything is in place: the signature seals the plist and the icon, so any
# later edit to the bundle invalidates it.
codesign --force --sign - "$app"
codesign --verify --strict --verbose=2 "$app"

# `COPYFILE_DISABLE`, or macOS tar adds `._` AppleDouble files for extended attributes, which
# land inside the bundle on extraction and break the signature's seal.
name="zuno-$version-aarch64-macos"
COPYFILE_DISABLE=1 tar -C "$out" -czf "$out/$name.tar.gz" Zuno.app
tar -tzvf "$out/$name.tar.gz"
echo "$out/$name.tar.gz"
