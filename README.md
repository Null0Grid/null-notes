# Null Notes (v0.6.0)

0.6.0 adds an opt-in backup to Null Drive (Phase 6). 0.5.0 added `nullnotes://` links (Phase 5). 0.4.0 added Phase 1 of cross-product search (see the Null Grid search contract v1). The rest of this file describes the 0.3.1 recovery.

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

## Phase 6: back up a vault to Null Drive (opt-in, one way)

Settings → **Back up to Null Drive**, off by default and set per vault. You enter the Drive address, an API token with the **Everything** scope (made in Drive under Settings → Devices → Add device → Create an API token instead), and a folder, `Null Notes/<vault name>` by default. *Test connection* checks the token, its scope and whether the account is encrypted.

- **One way.** Changes are copied local → Drive. Nothing on Drive is ever written back into the vault.
- **What's sent.** Every note and image, locked notes as they are (ciphertext), and `.nullnotes/*.json` (`ids.json`, `lock.json`, settings), which a restore needs. `*.nntmp` is never sent, nor are files the vault doesn't list. Empty folders are created on Drive too.
- **Only changes.** A per-vault record (`%LOCALAPPDATA%\dev.nullgrid.notes\backup\<hash>.json`) keeps size, mtime and SHA-256 for every file sent. A file is re-hashed only if its size or mtime changed, and uploaded only if the hash differs from Drive's. Uploads are `PUT /api/v1/files/{path}` with `If-Match: *`, `X-NullDrive-Mtime` (the note's mtime, UTC, ms), `X-Content-SHA256` and an explicit `Content-Length`. Drive keeps every replaced version.
- **Deletes** are `DELETE` with `If-Match: *`, so they land in Drive's Recently deleted for 30 days. A deleted folder is one delete. **In-app renames** are replayed as `POST /api/v1/files/move`, which keeps Drive's id and history without re-uploading.
- **When it runs.** At once when turned on and when the vault opens (catching up on changes made while the app was closed). After that, 20 s after the last change (the file watcher sees edits from Explorer, git and sync tools too), every 15 min as a safety net, and on *Back up now*.
- **Safety.**
  - If the vault folder is missing or empty (an unplugged drive, the wrong folder), nothing is deleted on Drive.
  - It only ever touches its own folder; a sibling like `Work2/` is never touched.
  - An end-to-end-encrypted Drive account is refused (`409 e2ee_required`), since a plaintext backup would defeat the encryption.
  - A Search-only token is refused.
  - A rejected token stops runs until a new one is pasted.
  - 5xx and network errors are retried (1 s, 4 s, 15 s), then retried again later with backoff up to 30 min.
  - Names Drive can't store (`CON.md`…) are skipped and listed under Settings.
- **Token.** Stored in Windows Credential Manager (`dev.nullgrid.notes` / `drive-backup:<server>`), never in `backup.json`. On Linux it's a `0600` file. `http://` is accepted only for this computer or a private network; anything else needs `https://`.
- **Status.** The status bar says `LOCAL + DRIVE · BACKED UP 3 MIN AGO` / `BACKUP ERROR` instead of `NO NETWORK` while backup is on. Settings shows the last run's counts and any problems.
- **Try it without Drive.** `python3 tools/mock_drive.py 8765 mytoken`, then use `http://127.0.0.1:8765`. `/_state` shows what arrived.

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
