#!/usr/bin/env bash
# Build the Null Notes .deb for this machine (amd64 or arm64). It runs on distros with a glibc at least
# as new as the build machine's (the release workflow builds on Ubuntu 22.04).
#
#   packaging/deb/build-deb.sh
#
# Needs: Rust, libwebkit2gtk-4.1-dev, libssl-dev, dpkg-deb.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
tauri="$root/src-tauri"
V="$(python3 -c "import json;print(json.load(open('$tauri/tauri.conf.json'))['version'])")"
A="$(dpkg --print-architecture)"
echo "Null Notes $V ($A)"

( cd "$tauri" && cargo build --release --features tauri/custom-protocol )

R="$here/null-notes_${V}_${A}"
rm -rf "$R"
mkdir -p "$R/DEBIAN" "$R/usr/bin" "$R/usr/share/applications" "$R/usr/share/doc/null-notes"
install -m755 "$tauri/target/release/null-notes" "$R/usr/bin/null-notes"
for s in 32 128 256 512; do mkdir -p "$R/usr/share/icons/hicolor/${s}x${s}/apps"; done
cp "$tauri/icons/32x32.png"      "$R/usr/share/icons/hicolor/32x32/apps/null-notes.png"
cp "$tauri/icons/128x128.png"    "$R/usr/share/icons/hicolor/128x128/apps/null-notes.png"
cp "$tauri/icons/128x128@2x.png" "$R/usr/share/icons/hicolor/256x256/apps/null-notes.png"
cp "$tauri/icons/icon.png"       "$R/usr/share/icons/hicolor/512x512/apps/null-notes.png"

cat > "$R/usr/share/applications/null-notes.desktop" <<'EOF'
[Desktop Entry]
Type=Application
Name=Null Notes
GenericName=Notes
Comment=A local markdown vault for notes, pages and images
Exec=null-notes %u
Icon=null-notes
Terminal=false
Categories=Office;
Keywords=notes;markdown;vault;nullgrid;
MimeType=x-scheme-handler/nullnotes;
StartupWMClass=null-notes
EOF

cat > "$R/usr/share/doc/null-notes/copyright" <<'EOF'
Null Notes
Copyright NULLGRID. All rights reserved.
EOF

size="$(du -sk "$R/usr" | cut -f1)"
# the glibc this binary actually needs (it depends on the distro it was built on)
glibc="$(objdump -T "$R/usr/bin/null-notes" | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -1 | cut -d_ -f2)"
cat > "$R/DEBIAN/control" <<EOF
Package: null-notes
Version: $V
Architecture: $A
Maintainer: NULLGRID <volosteele55@proton.me>
Installed-Size: $size
Depends: libc6 (>= $glibc), libwebkit2gtk-4.1-0, libjavascriptcoregtk-4.1-0, libsoup-3.0-0, libgtk-3-0t64 | libgtk-3-0, libglib2.0-0t64 | libglib2.0-0, libssl3t64 | libssl3, libdbus-1-3
Section: editors
Priority: optional
Homepage: https://nullgrid.dev
Description: Null Notes - a local markdown vault
 A local markdown vault for notes, pages and images, with links,
 a graph view, locked notes, Null Search integration and optional
 two-way sync with Null Drive. Handles nullnotes:// links.
EOF

dpkg-deb --root-owner-group --build "$R" >/dev/null
rm -rf "$R"
echo "Built $here/null-notes_${V}_${A}.deb"
