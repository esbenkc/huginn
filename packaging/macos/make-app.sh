#!/bin/sh
# Build Huginn.app so Spotlight, Launchpad and the Dock can open Huginn.
#
#   packaging/macos/make-app.sh [path/to/huginn] [install-dir]
#
# Defaults: target/release/huginn, installed into ~/Applications.
# The binary is copied into the bundle, so run this again after a rebuild.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
bin=${1:-$root/target/release/huginn}
dest=${2:-$HOME/Applications}
[ -x "$bin" ] || { echo "no huginn binary at $bin (build with: cargo build --release -p huginn-shell)" >&2; exit 1; }

version=$(sed -n 's/^version *= *"\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)
tmp=$(mktemp -d)
app="$tmp/Huginn.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"

# The filesystem is usually case-insensitive, so the binary needs a name that differs from "Huginn".
cp "$bin" "$app/Contents/MacOS/huginn-bin"
# Launched from Finder or Spotlight there are no arguments: open a window on the new tab page.
cat > "$app/Contents/MacOS/Huginn" <<'EOF'
#!/bin/sh
exec "$(dirname "$0")/huginn-bin" open "$@"
EOF
chmod +x "$app/Contents/MacOS/Huginn"

# Icon: icon-1024.png (made from icon.html with `huginn render`) -> .icns
set_dir="$tmp/Huginn.iconset"
mkdir "$set_dir"
for s in 16 32 128 256 512; do
  sips -z $s $s "$here/icon-1024.png" --out "$set_dir/icon_${s}x${s}.png" >/dev/null
  d=$((s * 2))
  sips -z $d $d "$here/icon-1024.png" --out "$set_dir/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$set_dir" -o "$app/Contents/Resources/Huginn.icns"

cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Huginn</string>
  <key>CFBundleDisplayName</key><string>Huginn</string>
  <key>CFBundleIdentifier</key><string>com.github.esbenkc.huginn</string>
  <key>CFBundleExecutable</key><string>Huginn</string>
  <key>CFBundleIconFile</key><string>Huginn</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.productivity</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
EOF

codesign --force --deep --sign - "$app" >/dev/null 2>&1 || true
mkdir -p "$dest"
rm -rf "$dest/Huginn.app"
mv "$app" "$dest/Huginn.app"
rm -rf "$tmp"
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f "$dest/Huginn.app" 2>/dev/null || true
mdimport "$dest/Huginn.app" 2>/dev/null || true
echo "installed $dest/Huginn.app"
