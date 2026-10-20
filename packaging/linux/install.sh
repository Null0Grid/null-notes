#!/usr/bin/env sh
# Install Null Notes from this folder.
#   ./install.sh            for you only, into ~/.local (no sudo)
#   sudo ./install.sh --system   for everyone, into /usr/local
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
if [ "${1:-}" = "--system" ]; then
  prefix=/usr/local
else
  prefix="$HOME/.local"
fi
bin="$prefix/bin"
apps="$prefix/share/applications"
icons="$prefix/share/icons/hicolor"

mkdir -p "$bin" "$apps"
install -m755 "$here/null-notes" "$bin/null-notes"
for s in 32 128 256 512; do
  mkdir -p "$icons/${s}x${s}/apps"
  install -m644 "$here/icons/${s}x${s}.png" "$icons/${s}x${s}/apps/null-notes.png"
done
# full path in Exec: ~/.local/bin isn't on every desktop's PATH
sed "s|^Exec=null-notes|Exec=$bin/null-notes|" "$here/null-notes.desktop" > "$apps/null-notes.desktop"
chmod 644 "$apps/null-notes.desktop"

command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database -q "$apps" 2>/dev/null || true
command -v gtk-update-icon-cache >/dev/null 2>&1 && gtk-update-icon-cache -q -t "$icons" 2>/dev/null || true
# nullnotes:// links open in Null Notes (for the user running this)
if [ "$prefix" != /usr/local ] && command -v xdg-mime >/dev/null 2>&1; then
  xdg-mime default null-notes.desktop x-scheme-handler/nullnotes 2>/dev/null || true
fi

echo "Installed Null Notes to $bin/null-notes."
echo "Find it in your app menu, or run: null-notes"
case ":$PATH:" in *":$bin:"*) ;; *) echo "(Note: $bin isn't on your PATH; the app menu entry works anyway.)";; esac
