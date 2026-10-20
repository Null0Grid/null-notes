#!/usr/bin/env bash
# Build the Linux packages of Null Notes.
#
#   packaging/linux/build-linux.sh [appimage] [rpm] [tar] [deb]     (default: all four)
#
# Output, in packaging/linux/dist/:
#   Null-Notes-<v>-x86_64.AppImage        runs on any distro, nothing to install
#   null-notes-<v>-1.x86_64.rpm           Fedora / openSUSE / RHEL-family
#   null-notes-<v>-linux-x86_64.tar.gz    plain binary + install.sh (installs into ~/.local)
#   null-notes_<v>_amd64.deb              Debian / Ubuntu (packaging/deb/build-deb.sh)
#
# A Linux binary only runs where glibc is at least as new as on the machine
# that built it. Build on the OLDEST distro you want to support: the release
# workflow uses Ubuntu 22.04 (glibc 2.35). The AppImage also needs the
# system's WebKitGTK 4.1 to be compatible, which is true on every current distro.
#
# Needs: Rust, the Tauri CLI (cargo install tauri-cli --version "^2"),
# libwebkit2gtk-4.1-dev, libssl-dev. AppImage builds download linuxdeploy.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
tauri="$root/src-tauri"
V="$(python3 -c "import json;print(json.load(open('$tauri/tauri.conf.json'))['version'])")"
want="${*:-appimage rpm tar deb}"
has() { [[ " $want " == *" $1 "* ]]; }
dist="$here/dist"
mkdir -p "$dist"
echo "Null Notes $V: $want"

# 1. AppImage and .rpm: Tauri's bundler (also builds the release binary)
bundles=()
has appimage && bundles+=(appimage)
has rpm && bundles+=(rpm)
if [ ${#bundles[@]} -gt 0 ]; then
  # linuxdeploy's bundled strip can't read newer ELF sections; GitHub runners have no FUSE
  export NO_STRIP=true APPIMAGE_EXTRACT_AND_RUN=1
  ( cd "$tauri" && cargo tauri build --bundles "$(IFS=,; echo "${bundles[*]}")" )
  out="$tauri/target/release/bundle"
  if has appimage; then
    cp "$(ls -t "$out"/appimage/*.AppImage | head -1)" "$dist/Null-Notes-$V-x86_64.AppImage"
    chmod +x "$dist/Null-Notes-$V-x86_64.AppImage"
  fi
  if has rpm; then
    cp "$(ls -t "$out"/rpm/*.rpm | head -1)" "$dist/null-notes-$V-1.x86_64.rpm"
  fi
else
  ( cd "$tauri" && cargo build --release --features tauri/custom-protocol )
fi

# 2. Plain .tar.gz: the binary, icons, a .desktop file and install/uninstall scripts
if has tar; then
  name="null-notes-$V-linux-x86_64"
  stage="$here/stage/$name"
  rm -rf "$here/stage"
  mkdir -p "$stage/icons"
  install -m755 "$tauri/target/release/null-notes" "$stage/null-notes"
  cp "$tauri/icons/32x32.png" "$stage/icons/32x32.png"
  cp "$tauri/icons/128x128.png" "$stage/icons/128x128.png"
  cp "$tauri/icons/128x128@2x.png" "$stage/icons/256x256.png"
  cp "$tauri/icons/icon.png" "$stage/icons/512x512.png"
  cp "$here/null-notes.desktop" "$stage/null-notes.desktop"
  install -m755 "$here/install.sh" "$stage/install.sh"
  install -m755 "$here/uninstall.sh" "$stage/uninstall.sh"
  sed "s/@VERSION@/$V/g" "$here/README-tarball.txt" > "$stage/README.txt"
  tar -C "$here/stage" --owner=0 --group=0 -czf "$dist/$name.tar.gz" "$name"
  rm -rf "$here/stage"
fi

# 3. .deb
if has deb; then
  "$root/packaging/deb/build-deb.sh"
  cp "$root/packaging/deb/null-notes_${V}_amd64.deb" "$dist/"
fi

glibc="$(objdump -T "$tauri/target/release/null-notes" | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -1)"
echo "Built (binary needs ${glibc/_/ }+):"
ls -la "$dist"
