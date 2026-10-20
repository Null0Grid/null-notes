#!/usr/bin/env sh
# Remove Null Notes installed by install.sh (your notes and settings are kept).
#   ./uninstall.sh            from ~/.local
#   sudo ./uninstall.sh --system   from /usr/local
set -eu
if [ "${1:-}" = "--system" ]; then prefix=/usr/local; else prefix="$HOME/.local"; fi
rm -f "$prefix/bin/null-notes" "$prefix/share/applications/null-notes.desktop"
for s in 32 128 256 512; do rm -f "$prefix/share/icons/hicolor/${s}x${s}/apps/null-notes.png"; done
command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database -q "$prefix/share/applications" 2>/dev/null || true
echo "Removed Null Notes from $prefix. Your vaults and settings (~/.config/dev.nullgrid.notes) were not touched."
