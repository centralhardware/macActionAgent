#!/bin/sh
# Builds a universal binary, wraps it in a .pkg and puts that in a .dmg.
# Run on macOS from the repo root. Output: target/mac-action-agent-<version>.dmg
set -eu
cd "$(dirname "$0")/.."
VERSION=${VERSION:-$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)}
ID=me.centralhardware.mac-action-agent

for t in aarch64-apple-darwin x86_64-apple-darwin; do
    cargo build --release --target "$t"
done

STAGE=target/pkgroot
rm -rf "$STAGE" target/dmg
mkdir -p "$STAGE/usr/local/libexec" "$STAGE/Library/LaunchAgents" target/dmg
lipo -create -output "$STAGE/usr/local/libexec/mac-action-agent" \
    target/aarch64-apple-darwin/release/mac-action-agent \
    target/x86_64-apple-darwin/release/mac-action-agent
# Sign with the shared LocalCodeSign certificate when CI provides one, so macOS keeps the
# binary's approvals (Accessibility, Full Disk Access, Automation) across updates instead of asking again each release.
BIN="$STAGE/usr/local/libexec/mac-action-agent"
if [ -n "${MACOS_CERT_P12:-}" ]; then
    KC="$PWD/target/signing.keychain-db"
    security delete-keychain "$KC" 2>/dev/null || true
    security create-keychain -p ci "$KC"
    security set-keychain-settings "$KC"
    security unlock-keychain -p ci "$KC"
    echo "$MACOS_CERT_P12" | base64 --decode > target/cert.p12
    security import target/cert.p12 -k "$KC" -P "$MACOS_CERT_PASSWORD" -T /usr/bin/codesign
    rm target/cert.p12
    security set-key-partition-list -S apple-tool:,apple: -s -k ci "$KC" >/dev/null
    # codesign only looks in the search list. The cert is self-signed (not trusted), so name it by hash.
    OLD_KCS=$(security list-keychains -d user | tr -d '"')
    security list-keychains -d user -s "$KC" $OLD_KCS
    HASH=$(security find-certificate -c "LocalCodeSign" -Z "$KC" | awk '/SHA-1/ {print $NF}')
    codesign --force --keychain "$KC" --sign "$HASH" --identifier "$ID" "$BIN"
    security list-keychains -d user -s $OLD_KCS
    security delete-keychain "$KC"
else
    codesign --force --sign - --identifier "$ID" "$BIN"
fi
cp packaging/$ID.plist "$STAGE/Library/LaunchAgents/"
chmod 755 packaging/scripts/* packaging/uninstall.command

pkgbuild --root "$STAGE" --scripts packaging/scripts \
    --identifier "$ID" --version "$VERSION" --install-location / \
    "target/dmg/Install mac-action-agent.pkg"
cp packaging/uninstall.command "target/dmg/Uninstall mac-action-agent.command"

hdiutil create -volname "mac-action-agent $VERSION" -srcfolder target/dmg \
    -ov -format UDZO "target/mac-action-agent-$VERSION.dmg"
