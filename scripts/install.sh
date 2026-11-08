#!/usr/bin/env bash
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN_DIR="$HOME/.local/bin"
DESKTOP_DIR="$HOME/.local/share/applications"
ICON_DIR="$HOME/.local/share/icons/hicolor/128x128/apps"

echo "⚡ [Illusion] Installing Illusion IDM for Arch Linux & Hyprland..."

# Ensure target directories exist
mkdir -p "$BIN_DIR" "$DESKTOP_DIR" "$ICON_DIR"

# 1. Build release binaries if not present
if [ ! -f "$SCRIPT_DIR/target/release/illusion" ]; then
    echo "📦 Building release binaries with Cargo..."
    cd "$SCRIPT_DIR" && cargo build --release
fi

# 2. Copy binaries to ~/.local/bin
echo "🚀 Installing binaries to $BIN_DIR..."
cp -f "$SCRIPT_DIR/target/release/illusion" "$BIN_DIR/illusion"
cp -f "$SCRIPT_DIR/target/release/illusion-cli" "$BIN_DIR/illusion-cli"
cp -f "$SCRIPT_DIR/target/release/illusion-native-host" "$BIN_DIR/illusion-native-host"
chmod +x "$BIN_DIR/illusion" "$BIN_DIR/illusion-cli" "$BIN_DIR/illusion-native-host"

# 3. Install App Icon
echo "🎨 Installing icon..."
cp -f "$SCRIPT_DIR/extension/icons/icon128.png" "$ICON_DIR/illusion.png"

# 4. Install Desktop Entry
echo "🖥️ Installing desktop entry to $DESKTOP_DIR/illusion.desktop..."
cat <<EOF > "$DESKTOP_DIR/illusion.desktop"
[Desktop Entry]
Name=Illusion
Comment=High-Performance IDM Alternative for Linux & Hyprland
Exec=$BIN_DIR/illusion %u
Icon=illusion
Terminal=false
Type=Application
Categories=Network;FileTransfer;
MimeType=x-scheme-handler/illusion;
StartupWMClass=illusion
EOF

update-desktop-database "$DESKTOP_DIR" 2>/dev/null || true

# 5. Register Native Messaging Hosts for Chromium, Google Chrome, and Firefox
echo "🌐 Registering browser native messaging hosts..."

CHROME_HOST_DIR="$HOME/.config/google-chrome/NativeMessagingHosts"
CHROMIUM_HOST_DIR="$HOME/.config/chromium/NativeMessagingHosts"
MOZILLA_HOST_DIR="$HOME/.mozilla/native-messaging-hosts"

mkdir -p "$CHROME_HOST_DIR" "$CHROMIUM_HOST_DIR" "$MOZILLA_HOST_DIR"

# Chrome/Chromium manifest (Chromium requires EXACT extension IDs with trailing slashes, NO wildcards!)
EXT_ID="jfnhbpdihmchildojijlbjlldbenpdhh"

# Try detecting dynamic ID from Chromium / Chrome Preferences if present
DETECTED_ID=$(python3 -c '
import json, os
for p in ["'$HOME'/.config/chromium/Default/Preferences", "'$HOME'/.config/google-chrome/Default/Preferences"]:
    if os.path.exists(p):
        try:
            with open(p) as f:
                d = json.load(f)
            for k, v in d.get("extensions", {}).get("settings", {}).items():
                if "illusion" in str(v.get("path", "")).lower() or "illusion" in str(v.get("manifest", {}).get("name", "")).lower():
                    print(k)
                    break
        except Exception:
            pass
' 2>/dev/null | head -n 1)

if [ -n "$DETECTED_ID" ]; then
    EXT_ID="$DETECTED_ID"
fi

cat <<EOF > "$CHROMIUM_HOST_DIR/com.illusion.downloader.json"
{
  "name": "com.illusion.downloader",
  "description": "Illusion Native Messaging Host",
  "path": "$BIN_DIR/illusion-native-host",
  "type": "stdio",
  "allowed_origins": [
    "chrome-extension://${EXT_ID}/",
    "chrome-extension://jfnhbpdihmchildojijlbjlldbenpdhh/"
  ]
}
EOF

cp -f "$CHROMIUM_HOST_DIR/com.illusion.downloader.json" "$CHROME_HOST_DIR/com.illusion.downloader.json"

# Firefox manifest
cat <<EOF > "$MOZILLA_HOST_DIR/com.illusion.downloader.json"
{
  "name": "com.illusion.downloader",
  "description": "Illusion Native Messaging Host",
  "path": "$BIN_DIR/illusion-native-host",
  "type": "stdio",
  "allowed_extensions": [
    "illusion@archlinux.org"
  ]
}
EOF

# 6. Install Hyprland window rules
HYPR_CONFIG_DIR="$HOME/.config/hypr"
if [ -d "$HYPR_CONFIG_DIR" ]; then
    echo "🪟 Found Hyprland configuration at $HYPR_CONFIG_DIR"
    cp -f "$SCRIPT_DIR/hyprland/illusion.conf" "$HYPR_CONFIG_DIR/illusion.conf"
    echo "   Installed Hyprland rules to: $HYPR_CONFIG_DIR/illusion.conf"
    echo "   Add 'source = ~/.config/hypr/illusion.conf' to your hyprland.conf if not already included."
fi

echo ""
echo "✅ Illusion installation completed successfully!"
echo "   Run 'illusion' to launch the GUI or 'illusion-cli --help' for command line usage."
echo "   Load the extension located at: $SCRIPT_DIR/extension in Chromium or Chrome."
