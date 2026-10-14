#!/bin/bash
# Build Snip.app (universal: Apple silicon + Intel) and a drag-to-install Snip.dmg in dist/.
# Usage: scripts/bundle.sh            ad-hoc signed (fine for yourself / testers)
#        SIGN_ID="Developer ID Application: Name (TEAMID)" scripts/bundle.sh   for public release
set -euo pipefail
cd "$(dirname "$0")/.."

NAME=Snip
BUNDLE_ID=${BUNDLE_ID:-com.satyaadhiyaksa.snip}
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
APP=dist/$NAME.app

cargo build --release --target aarch64-apple-darwin
cargo build --release --target x86_64-apple-darwin

rm -rf dist && mkdir -p "$APP/Contents/MacOS"
lipo -create -output "$APP/Contents/MacOS/snip" \
    target/aarch64-apple-darwin/release/snip target/x86_64-apple-darwin/release/snip

cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>$NAME</string>
    <key>CFBundleDisplayName</key><string>$NAME</string>
    <key>CFBundleIdentifier</key><string>$BUNDLE_ID</string>
    <key>CFBundleVersion</key><string>$VERSION</string>
    <key>CFBundleShortVersionString</key><string>$VERSION</string>
    <key>CFBundleExecutable</key><string>snip</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>LSMinimumSystemVersion</key><string>12.0</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>LSUIElement</key><true/>
</dict>
</plist>
EOF

# "-" = ad-hoc signature. A Developer ID identity is required for Gatekeeper-friendly distribution.
codesign --force --deep --options runtime --sign "${SIGN_ID:--}" "$APP"

# DMG with an Applications shortcut so users can drag-install.
STAGE=$(mktemp -d)
cp -R "$APP" "$STAGE/" && ln -s /Applications "$STAGE/Applications"
hdiutil create -quiet -volname "$NAME" -srcfolder "$STAGE" -ov -format UDZO "dist/$NAME.dmg"
rm -rf "$STAGE"

echo "Built $APP and dist/$NAME.dmg ($VERSION)"
