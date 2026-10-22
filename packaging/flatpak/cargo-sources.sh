#!/usr/bin/env bash
# Generate packaging/flatpak/cargo-sources.json (the crates the offline Flatpak
# build needs) from src-tauri/Cargo.lock, with flatpak-builder-tools' generator.
# Run it again whenever Cargo.lock changes.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
tools="${TMPDIR:-/tmp}/flatpak-builder-tools"
if [ ! -f "$tools/cargo/flatpak-cargo-generator.py" ]; then
  rm -rf "$tools"
  git clone --quiet --depth 1 https://github.com/flatpak/flatpak-builder-tools.git "$tools"
fi
python3 -c "import aiohttp, tomlkit" 2>/dev/null || pip install --quiet aiohttp tomlkit 2>/dev/null \
  || pip install --quiet --break-system-packages aiohttp tomlkit
python3 "$tools/cargo/flatpak-cargo-generator.py" "$root/src-tauri/Cargo.lock" -o "$here/cargo-sources.json"
echo "Wrote $here/cargo-sources.json ($(grep -c '"type": "archive"' "$here/cargo-sources.json") crates)"
