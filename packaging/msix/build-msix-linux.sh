#!/usr/bin/env bash
# Build the Null Notes Microsoft Store package on Linux (no Windows needed).
#
#   packaging/msix/build-msix-linux.sh [--sign test.pfx PASSWORD]
#
# Needs: Rust + the rust-src component, mingw-w64, and Microsoft's open-source
# makemsix (github.com/microsoft/msix-packaging, ./makelinux.sh --pack) on PATH
# or in $MAKEMSIX. Cross-compiles with -Zbuild-std, because rustup's prebuilt
# windows std may not be downloadable everywhere (needs rust-src instead).
#
# Without Windows' makepri there is no resources.pri, so only the unqualified
# logos the manifest names are packed (scaled variants are left out). For the
# full scaled set, use build-msix.ps1 on Windows.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
tauri="$root/src-tauri"
publisher="${PUBLISHER:-CN=8685350B-5C4C-4422-9CDF-8AEB87F50B2F}"
makemsix="${MAKEMSIX:-$(command -v makemsix || true)}"
[ -x "$makemsix" ] || { echo "makemsix not found (set MAKEMSIX)"; exit 1; }

version="$(python3 -c "import json,sys;v=json.load(open('$tauri/tauri.conf.json'))['version'].split('-')[0].split('+')[0].split('.');v+=['0']*(3-len(v));print('.'.join(v[:3])+'.0')")"
echo "Null Notes $version"

target=x86_64-pc-windows-gnu
( cd "$tauri" && RUSTC_BOOTSTRAP=1 \
  CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc \
  AR_x86_64_pc_windows_gnu=x86_64-w64-mingw32-ar \
  cargo build --release -Zbuild-std=std,panic_abort --target $target --features tauri/custom-protocol )

out="$tauri/target/$target/release"
stage="$here/stage"
rm -rf "$stage"; mkdir -p "$stage/Assets"
cp "$out/null-notes.exe" "$stage/"
loader="$(find "$out" -name WebView2Loader.dll -path '*x64*' | head -1)"
[ -n "$loader" ] || loader="$(find "$out" -name WebView2Loader.dll | head -1)"
[ -n "$loader" ] && cp "$loader" "$stage/"
for a in Square150x150Logo Square44x44Logo StoreLogo Wide310x150Logo; do cp "$here/Assets/$a.png" "$stage/Assets/"; done
sed -e "s/{{VERSION}}/$version/" -e "s/{{PUBLISHER}}/$publisher/" "$here/AppxManifest.xml" > "$stage/AppxManifest.xml"

pkg="$here/NullNotes_${version}_x64.msix"
rm -f "$pkg"
"$makemsix" pack -d "$stage" -p "$pkg"
echo "Built $pkg (unsigned: upload this one to Partner Center)"

if [ "${1:-}" = "--sign" ]; then
  signed="${pkg%.msix}_test-signed.msix"
  osslsigncode sign -pkcs12 "$2" -pass "$3" -h sha256 -in "$pkg" -out "$signed"
  echo "Built $signed (for a local install test only)"
fi
