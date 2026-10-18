# Null Notes (v0.5.0)

0.5.0 adds `nullnotes://` links (Phase 5). 0.4.0 added Phase 1 of cross-product search (see the Null Grid search contract v1). The rest of this file describes the 0.3.1 recovery.

## Phase 1: search agent

- **Note ids.** `.md` notes get `id: <uuid>` in frontmatter, backfilled on first open with modified times kept. The editor never sees the id: it is stripped on read and restored on save. `.txt`, `.html` and `.canvas` ids live in `.nullnotes/ids.json` and follow in-app renames, moves, deletes and lock/unlock. If two files share an id (a copy made in Explorer), the copy gets a new one.
- **Index.** SQLite FTS5 trigram, one DB per vault in `%LOCALAPPDATA%\dev.nullgrid.notes\index\`, never inside the vault. The indexed columns are normalized (lowercase, letters and digits only), so `libre ai`, `Libre-AI` and `LibreAI` all match. Matching runs on the whole query, then on every word in any order. Frontmatter is searchable but kept out of snippets. HTML is indexed without tags; canvas by card text. Locked notes are never indexed.
- **Watcher.** notify, 600 ms debounce, incremental by mtime and size. It catches changes from the app, Explorer, git and sync tools.
- **Agent.** `127.0.0.1:47821` only, `GET /search?q=&limit=` (default 20, max 100). Requires `X-Null-Token` (401). The Host header must be 127.0.0.1 or localhost (403, blocks DNS rebinding). A request with an Origin header is answered only for the configured Null Search origin (403 otherwise), and the preflight returns `Access-Control-Allow-Private-Network: true`. If no vault is open, it returns `[]`.
- **Config.** `%APPDATA%\dev.nullgrid.notes\agent.json` holds `{ token, origin }`. The token is 32 random bytes in hex, made on first run. Settings → Null Search shows, copies and regenerates it and sets the origin.

```
curl -H "X-Null-Token: <token>" "http://127.0.0.1:47821/search?q=libre%20ai"
```

## Phase 5: `nullnotes://open?id=<uuid>` links

- **What a link does.** It opens the note in the desktop app. Lookup order: the open vault's index, a scan of the open vault (`ids.json`, then `.md` frontmatter), then your other recent vaults. If the note is in another vault, the app saves and locks the current one, switches, and opens it. Links can come from any web page, so only `open` with a valid uuid is accepted. A link can't create, change or delete anything. A bad or unknown link shows a toast.
- **Already running.** `tauri-plugin-single-instance` (registered first) forwards the link to the running copy and the new process exits, so there is never a second app fighting over port 47821. A cold start reads the link from the command line (`deep-link` `get_current`).
- **NSIS / MSI install.** `plugins.deep-link.desktop.schemes = ["nullnotes"]` makes the Tauri bundler write `Software\Classes\nullnotes` (`"null-notes.exe" "%1"`) on install and remove it on uninstall. Dev builds register themselves at startup.
- **Microsoft Store (MSIX).** Registry writes don't register protocols for packaged apps; the manifest does. `packaging/msix/AppxManifest.xml` declares `uap3:Protocol Name="nullnotes" Parameters="&quot;%1&quot;"`. The link must be the only argument, because that's what the deep-link plugin accepts. `packaging/msix/build-msix.ps1` builds the release exe and fills in the version (x.y.z.0) from `tauri.conf.json`. It also generates `resources.pri` for the scaled logos and packs `NullNotes_<ver>_x64.msix`; `-Sign -CertPath` signs it for a local install test. Identity `NULLGRID.NullNotes` and the publisher come from your 0.3.1 package; check them against Partner Center → Product identity.

Test a link on Windows: `start nullnotes://open?id=<id>`, or from PowerShell `Start-Process "nullnotes://open?id=<id>"`.

---

# Recovered source (v0.3.1)

Rebuilt from the shipped `Null_Notes_0.3.1_x64-setup.exe` on 2026-10-18.

## What's original vs rewritten

| Part | Status |
|---|---|
| `src/` (HTML, JS, CSS, fonts, vendor libs) | **Original.** Extracted byte-for-byte from the brotli asset table inside `null-notes.exe`. It was never minified, so this is your source. |
| `src-tauri/tauri.conf.json`, capabilities | Reconstructed from the config Tauri bakes into the binary (identifier, version, CSP, icons, author). Window size is a guess (1280×800); it isn't stored as text. |
| `src-tauri/src/*.rs` | **Rewritten.** Rust compiles to machine code, so the backend was re-implemented from the 31 commands the frontend calls, the error messages in the binary, and disassembly of the lock code. |
| `src-tauri/icons/` | Extracted from the exe's icon resource. |

## On-disk formats (kept compatible with the shipped app)

- App settings: `%APPDATA%\dev.nullgrid.notes\settings.json` → `{ "last": path|null, "recent": [paths] }` (max 10)
- Per-vault settings: `<vault>/.nullnotes/<name>.json` (`settings`, `graph`), name `[a-z0-9-]{1,40}`, ≤ 2 MB
- Saves are atomic: write `<file>.nntmp`, then rename
- Lock: `<vault>/.nullnotes/lock.json`
  `{ version: 1, kdf: "argon2id", m_kib: 65536, t: 3, p: 1, salt: b64(16), wrapped: b64(nonce|ct|tag) }`
  KEK = Argon2id v19 → 32 bytes; wrapped = XChaCha20-Poly1305(KEK, 32-byte data key, AAD `null-notes data key v1`)
- Locked note: `<note>.<ext>.lock` = `NNLOCK1\n` | nonce(24) | ciphertext | tag(16), AAD `NNLOCK1\n`
- Limits: edit 5 MB, images/imports 50 MB

The lock format was taken from disassembly, not tested against a file the
real app produced. Before trusting it with notes you care about, lock a
throwaway note in 0.3.1 and open it with this build.

## Build

```
cd src-tauri
cargo test --lib          # backend unit tests
cargo tauri build         # needs tauri-cli: cargo install tauri-cli --version "^2"
```
