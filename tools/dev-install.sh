#!/bin/sh
# Build Linen and install a stripped copy for the app launcher:
# ~/.local/share/linen/app/linen, linked from ~/.local/bin/linen, plus the menu entry.
set -e
cd "$(dirname "$0")/.."
cargo build
app="$HOME/.local/share/linen/app"
mkdir -p "$app" "$HOME/.local/bin" "$HOME/.local/share/applications"
strip -o "$app/linen.new" target/debug/linen
mv "$app/linen.new" "$app/linen"
ln -sf "$app/linen" "$HOME/.local/bin/linen"
# the full path: app launchers don't always have ~/.local/bin on their PATH
sed "s|^Exec=linen|Exec=$app/linen|" packaging/linen.desktop > "$HOME/.local/share/applications/linen.desktop"
# the icon (a play button drawn as one thread)
icons="$HOME/.local/share/icons/hicolor"
mkdir -p "$icons/scalable/apps"
cp packaging/linen.svg "$icons/scalable/apps/linen.svg"
for n in 16 24 32 48 64 128 256; do
    mkdir -p "$icons/${n}x${n}/apps"
    rsvg-convert -w $n -h $n packaging/linen.svg -o "$icons/${n}x${n}/apps/linen.png" 2>/dev/null || true
done
gtk-update-icon-cache -q "$icons" 2>/dev/null || true
update-desktop-database "$HOME/.local/share/applications" 2>/dev/null || true
echo "installed: $(du -h "$app/linen" | cut -f1)"
