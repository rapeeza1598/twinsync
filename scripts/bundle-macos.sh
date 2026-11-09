#!/usr/bin/env bash
# Builds a release binary and wraps it as TwinSync.app (ad-hoc signed).
#
#   scripts/bundle-macos.sh                      -> target/bundle/TwinSync.app
#   BUNDLE_ID=com.yourcompany.twinsync scripts/bundle-macos.sh
#
# The default bundle identifier is a development placeholder. An ad-hoc signature is
# enough to run the app on this Mac; distributing it to other Macs needs a Developer ID
# certificate and notarization (see README).
set -euo pipefail

export PATH="$HOME/.cargo/bin:$PATH"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

BUNDLE_ID="${BUNDLE_ID:-com.example.twinsync}"
VERSION="$(cargo pkgid | sed 's/.*[#@]//')"
APP="$ROOT/target/bundle/TwinSync.app"

cargo build --release
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/twinsync "$APP/Contents/MacOS/twinsync"
sed -e "s|@BUNDLE_ID@|$BUNDLE_ID|g" -e "s|@VERSION@|$VERSION|g" \
    packaging/macos/Info.plist > "$APP/Contents/Info.plist"
plutil -lint "$APP/Contents/Info.plist"

codesign --force --deep --sign - "$APP"
codesign --verify --deep --strict --verbose=2 "$APP"
echo "Built $APP (bundle id $BUNDLE_ID, version $VERSION, ad-hoc signed)"
