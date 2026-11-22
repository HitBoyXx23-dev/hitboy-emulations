#!/usr/bin/env bash
# Builds HitBoy's Emulations for macOS or Linux and packages it in dist/.
#
#   macOS:  dist/<Brand> Emulations.app  and  dist/<Brand>-Emulations-macOS.zip
#   Linux:  dist/<Brand> Emulations/     and  dist/<Brand>-Emulations-Linux.tar.gz
#
# Requirements
#   All:    Rust (https://rustup.rs) and a C++ compiler
#   macOS:  Xcode command-line tools (xcode-select --install); QEMU for VMs: brew install qemu
#   Linux:  sudo apt install build-essential pkg-config libasound2-dev libudev-dev libgtk-3-dev \
#                            libxkbcommon-dev libwayland-dev curl
#           QEMU for VMs: sudo apt install qemu-system-x86 qemu-utils ovmf
set -euo pipefail
cd "$(dirname "$0")"

cargo build --release

os="$(uname -s)"
version="$(sed -n 's/^version *= *"\(.*\)"/\1/p' crates/frontend/Cargo.toml | head -1)"
mkdir -p dist

brands=("hitboy:HitBoy's")
for entry in "${brands[@]}"; do
    id="${entry%%:*}"
    name="${entry#*:}"
    full="$name Emulations"
    safe="${name//\'/}-Emulations"
    bin="target/release/$id"

    if [ "$os" = "Darwin" ]; then
        app="dist/$full.app"
        rm -rf "$app"
        mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
        cp "$bin" "$app/Contents/MacOS/$id"
        # App icon from the brand mark.
        "$bin" --export-logo "$app/Contents/Resources" >/dev/null 2>&1 || true
        if command -v sips >/dev/null && command -v iconutil >/dev/null && [ -f "$app/Contents/Resources/$id-icon.png" ]; then
            set_dir="$(mktemp -d)/icon.iconset"
            mkdir -p "$set_dir"
            for s in 16 32 64 128 256 512; do
                sips -z $s $s "$app/Contents/Resources/$id-icon.png" --out "$set_dir/icon_${s}x${s}.png" >/dev/null
            done
            iconutil -c icns "$set_dir" -o "$app/Contents/Resources/icon.icns"
        fi
        cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>$full</string>
  <key>CFBundleDisplayName</key><string>$full</string>
  <key>CFBundleIdentifier</key><string>app.$id.emulations</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleExecutable</key><string>$id</string>
  <key>CFBundleIconFile</key><string>icon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
</dict></plist>
PLIST
        (cd dist && rm -f "$safe-macOS.zip" && zip -qry "$safe-macOS.zip" "$full.app")
        echo "$full -> $app, dist/$safe-macOS.zip"
    else
        dir="dist/$full"
        rm -rf "$dir"
        mkdir -p "$dir"
        cp "$bin" "$dir/$id"
        "$bin" --export-logo "$dir" >/dev/null 2>&1 || true
        cat > "$dir/$id.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=$full
Exec=$id
Icon=$id-icon
Categories=Game;Emulator;
DESKTOP
        cat > "$dir/install.sh" <<'INSTALL'
#!/usr/bin/env bash
# Installs the app for the current user (menu entry + icon).
set -e
here="$(cd "$(dirname "$0")" && pwd)"
for f in "$here"/*.desktop; do
    id="$(basename "$f" .desktop)"
    mkdir -p ~/.local/bin ~/.local/share/applications ~/.local/share/icons
    ln -sf "$here/$id" ~/.local/bin/"$id"
    cp "$here/$id-icon.png" ~/.local/share/icons/ 2>/dev/null || true
    cp "$f" ~/.local/share/applications/
done
echo "Installed. Find it in your applications menu."
INSTALL
        chmod +x "$dir/install.sh" "$dir/$id"
        tar -C dist -czf "dist/$safe-Linux.tar.gz" "$full"
        echo "$full -> $dir, dist/$safe-Linux.tar.gz"
    fi
done
