#!/usr/bin/env bash
# Construit CleanSpace.app (double-cliquable, avec son icône).
#
#   ./scripts/build-app.sh               # architecture de votre Mac
#   ./scripts/build-app.sh --universal   # Apple Silicon + Intel
#
# Le résultat est dans target/CleanSpace.app
set -euo pipefail

cd "$(dirname "$0")/.."

if [[ "$(uname)" != "Darwin" ]]; then
  echo "Ce script doit être lancé sous macOS." >&2
  exit 1
fi

BIN=clean-uninstall-gui
APP=target/CleanSpace.app
VERSION=$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')

if [[ "${1:-}" == "--universal" ]]; then
  rustup target add aarch64-apple-darwin x86_64-apple-darwin
  cargo build --release --bin "$BIN" --target aarch64-apple-darwin
  cargo build --release --bin "$BIN" --target x86_64-apple-darwin
  mkdir -p target/universal
  lipo -create -output "target/universal/$BIN" \
    "target/aarch64-apple-darwin/release/$BIN" \
    "target/x86_64-apple-darwin/release/$BIN"
  BIN_PATH="target/universal/$BIN"
else
  cargo build --release --bin "$BIN"
  BIN_PATH="target/release/$BIN"
fi

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN_PATH" "$APP/Contents/MacOS/$BIN"
cp assets/CleanSpace.icns "$APP/Contents/Resources/CleanSpace.icns"
sed "s/__VERSION__/$VERSION/g" assets/Info.plist.template > "$APP/Contents/Info.plist"

# Signature ad hoc : suffisante pour un usage local (pas de notarisation).
codesign --force --deep --sign - "$APP"

echo "✅ $APP (version $VERSION)"
echo "   Premier lancement : clic droit > Ouvrir si macOS signale un développeur non identifié."
