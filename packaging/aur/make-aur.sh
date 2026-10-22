#!/usr/bin/env bash
# Write the AUR files for null-notes-bin from the release tarballs.
#
#   packaging/aur/make-aur.sh <dir with both linux tarballs> [out dir]
#
# Writes <out>/PKGBUILD and <out>/.SRCINFO. .SRCINFO comes from
# `makepkg --printsrcinfo` when makepkg is there (Arch), else it's written
# here in the same format.
# To publish: git clone ssh://aur@aur.archlinux.org/null-notes-bin.git,
# copy both files in, commit, push.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
src="$1"; out="${2:-$here/out}"
V="$(python3 -c "import json;print(json.load(open('$root/src-tauri/tauri.conf.json'))['version'])")"
sum() { sha256sum "$src/null-notes-$V-linux-$1.tar.gz" | cut -d' ' -f1; }
x="$(sum x86_64)"; a="$(sum aarch64)"
mkdir -p "$out"
sed -e "s/@VERSION@/$V/g" -e "s/@SHA_X86_64@/$x/" -e "s/@SHA_AARCH64@/$a/" "$here/PKGBUILD.in" > "$out/PKGBUILD"
if command -v makepkg >/dev/null; then
  ( cd "$out" && makepkg --printsrcinfo > .SRCINFO )
else
  u="https://github.com/Null0Grid/null-notes/releases/download/$V"
  {
    printf 'pkgbase = null-notes-bin\n'
    printf '\tpkgdesc = %s\n' "$(sed -n 's/^pkgdesc="\(.*\)"$/\1/p' "$here/PKGBUILD.in")"
    printf '\tpkgver = %s\n\tpkgrel = 1\n\turl = https://nullgrid.dev\n' "$V"
    printf '\tarch = x86_64\n\tarch = aarch64\n\tlicense = LicenseRef-NULLGRID\n'
    for d in webkit2gtk-4.1 gtk3 libsoup3 glib2 openssl cairo pango gdk-pixbuf2 hicolor-icon-theme gcc-libs glibc; do
      printf '\tdepends = %s\n' "$d"
    done
    printf '\tprovides = null-notes\n\tconflicts = null-notes\n\toptions = !strip\n\toptions = !debug\n'
    printf '\tsource_x86_64 = %s/null-notes-%s-linux-x86_64.tar.gz\n\tsha256sums_x86_64 = %s\n' "$u" "$V" "$x"
    printf '\tsource_aarch64 = %s/null-notes-%s-linux-aarch64.tar.gz\n\tsha256sums_aarch64 = %s\n' "$u" "$V" "$a"
    printf '\npkgname = null-notes-bin\n'
  } > "$out/.SRCINFO"
fi
echo "Wrote $out/PKGBUILD and $out/.SRCINFO ($V)"
