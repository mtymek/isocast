#!/bin/sh
# Install a desktop entry for the development build, so the desktop's
# portals know Isocast by its app id (dev.isocast.Isocast): GlobalShortcuts
# refuses host apps without one. Removing the file undoes it:
#   rm ~/.local/share/applications/dev.isocast.Isocast.desktop
set -eu
repo=$(cd "$(dirname "$0")/.." && pwd)
dir=${XDG_DATA_HOME:-$HOME/.local/share}/applications
mkdir -p "$dir"
cat > "$dir/dev.isocast.Isocast.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=Isocast
Comment=Scene-based recording with per-source ISO files
Exec=$repo/target/release/isocast-ui
Icon=video-display
Categories=AudioVideo;Video;Recorder;
Terminal=false
StartupWMClass=isocast-ui
DESKTOP
command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$dir" || true
echo "Installed $dir/dev.isocast.Isocast.desktop"
