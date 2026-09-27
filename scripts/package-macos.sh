#!/bin/sh
set -eu

if [ "$(uname -s)" != Darwin ]; then
    printf '%s\n' 'macOS app packaging requires macOS.' >&2
    exit 1
fi

sideport_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
sideport_profile=${1:-debug}
cd "$sideport_root"

case "$sideport_profile" in
    debug) "$sideport_root/scripts/cargo-ui.sh" build -p sl-app ;;
    release) "$sideport_root/scripts/cargo-ui.sh" build -p sl-app --release ;;
    *) printf '%s\n' 'Usage: scripts/package-macos.sh [debug|release]' >&2; exit 2 ;;
esac

sideport_bundle="$sideport_root/target/$sideport_profile/Sideport.app"
mkdir -p "$sideport_bundle/Contents/MacOS"
cp "$sideport_root/target/$sideport_profile/SideportDesktop" "$sideport_bundle/Contents/MacOS/SideportDesktop"
rm -f "$sideport_bundle/Contents/MacOS/Sideport"

cat > "$sideport_bundle/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key><string>com.sideport.desktop</string>
    <key>CFBundleName</key><string>Sideport</string>
    <key>CFBundleDisplayName</key><string>Sideport</string>
    <key>CFBundleExecutable</key><string>SideportDesktop</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>0.1.0</string>
    <key>CFBundleVersion</key><string>1</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSPrincipalClass</key><string>NSApplication</string>
    <key>LSMinimumSystemVersion</key><string>10.15</string>
</dict>
</plist>
PLIST

printf '%s\n' "$sideport_bundle"
