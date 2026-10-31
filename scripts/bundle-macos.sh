#!/usr/bin/env bash
# Build a universal (arm64 + x86_64) Docsqueeze.app and .dmg.
# Usage: scripts/bundle-macos.sh [out-dir]   (default: dist)
set -euo pipefail

cd "$(dirname "$0")/.."
out="${1:-dist}"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
targets=(aarch64-apple-darwin x86_64-apple-darwin)

for target in "${targets[@]}"; do
  rustup target add "$target" >/dev/null
  # Separate builds: with the GUI feature on, the CLI would also link every
  # image format dioxus enables.
  cargo build --release --locked --bin docsqueeze_cli --target "$target"
  cargo build --release --locked --features gui --bin Docsqueeze --target "$target"
done

rm -rf "$out/macos"
mkdir -p "$out/macos"
for bin in docsqueeze_cli Docsqueeze; do
  lipo -create -output "$out/macos/$bin" \
    target/aarch64-apple-darwin/release/"$bin" target/x86_64-apple-darwin/release/"$bin"
done

app="$out/macos/Docsqueeze.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
mv "$out/macos/Docsqueeze" "$app/Contents/MacOS/Docsqueeze"
cp assets/icon.icns "$app/Contents/Resources/Docsqueeze.icns"
cat >"$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Docsqueeze</string>
  <key>CFBundleDisplayName</key><string>Docsqueeze</string>
  <key>CFBundleExecutable</key><string>Docsqueeze</string>
  <key>CFBundleIconFile</key><string>Docsqueeze.icns</string>
  <key>CFBundleIdentifier</key><string>io.github.rustysky.docsqueeze</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
EOF

# Ad-hoc signature: unsigned arm64 apps are reported as "damaged" when downloaded.
codesign --force --deep --sign - "$app"

dmg_root="$out/macos/dmg"
mkdir -p "$dmg_root"
cp -R "$app" "$dmg_root/"
ln -s /Applications "$dmg_root/Applications"
hdiutil create -volname Docsqueeze -srcfolder "$dmg_root" -ov -format UDZO \
  "$out/docsqueeze-$version-macos-universal.dmg" >/dev/null
rm -rf "$dmg_root"

tar -C "$out/macos" -czf "$out/docsqueeze-cli-$version-macos-universal.tar.gz" docsqueeze_cli
echo "Built:"
ls -1 "$out"/*.dmg "$out"/*.tar.gz
