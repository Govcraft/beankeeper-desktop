#!/bin/bash
# Optional paid-signing hook. Called after unsigned pkg creation, before archiving.
set -euo pipefail
if [[ "${ENABLE_APPLE_SIGNING:-false}" != true ]]; then
    echo 'Apple signing/notarization skipped: disabled; Apple Developer account is pending. Keeping unsigned pkg.'
    exit 0
fi
for variable in APPLE_CERTIFICATES_P12_BASE64 APPLE_CERTIFICATES_PASSWORD APPLE_APPLICATION_IDENTITY APPLE_INSTALLER_IDENTITY APPLE_NOTARY_KEY_BASE64 APPLE_NOTARY_KEY_ID APPLE_NOTARY_ISSUER; do
    if [[ -z "${!variable:-}" ]]; then
        echo "Apple signing/notarization skipped: missing $variable. Keeping unsigned pkg."
        exit 0
    fi
done
stage=$1
version=$2
package=$3
temporary=$(mktemp -d)
keychain="$temporary/signing.keychain-db"
cleanup() {
    security delete-keychain "$keychain" >/dev/null 2>&1 || true
    rm -rf "$temporary"
}
trap cleanup EXIT
printf '%s' "$APPLE_CERTIFICATES_P12_BASE64" | base64 --decode > "$temporary/certificates.p12"
printf '%s' "$APPLE_NOTARY_KEY_BASE64" | base64 --decode > "$temporary/notary.p8"
password=$(openssl rand -hex 32)
security create-keychain -p "$password" "$keychain"
security unlock-keychain -p "$password" "$keychain"
security import "$temporary/certificates.p12" -k "$keychain" -P "$APPLE_CERTIFICATES_PASSWORD" -T /usr/bin/codesign -T /usr/bin/productsign -T /usr/bin/pkgbuild
security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$password" "$keychain" >/dev/null
codesign --force --options runtime --timestamp --keychain "$keychain" --sign "$APPLE_APPLICATION_IDENTITY" "$stage/bk"
codesign --verify --strict --verbose=2 "$stage/bk"
mkdir -p "$temporary/root/usr/local/bin"
cp "$stage/bk" "$temporary/root/usr/local/bin/bk"
mkdir -p "$temporary/root/usr/local/share/doc/beankeeper-desktop"
cp "$stage/README.md" "$stage/LICENSE.md" "$temporary/root/usr/local/share/doc/beankeeper-desktop/"
mkdir -p "$temporary/scripts"
cp "$(dirname "$0")/../packaging/postinstall" "$temporary/scripts/postinstall"
pkgbuild --root "$temporary/root" --scripts "$temporary/scripts" --identifier ai.govcraft.beankeeper-desktop --version "$version" --install-location / "$temporary/unsigned.pkg"
productsign --keychain "$keychain" --sign "$APPLE_INSTALLER_IDENTITY" "$temporary/unsigned.pkg" "$temporary/signed.pkg"
xcrun notarytool submit "$temporary/signed.pkg" --key "$temporary/notary.p8" --key-id "$APPLE_NOTARY_KEY_ID" --issuer "$APPLE_NOTARY_ISSUER" --wait
xcrun stapler staple "$temporary/signed.pkg"
xcrun stapler validate "$temporary/signed.pkg"
pkgutil --check-signature "$temporary/signed.pkg"
cp "$temporary/signed.pkg" "$package"
