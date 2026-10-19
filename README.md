# Null Notes (v5.0.0)

5.0.0 adds backup and two-way sync into end-to-end encrypted Null Drive accounts (Phase 8). 0.7.0 added two-way sync with Null Drive (Phase 7). 0.6.0 added an opt-in backup to Null Drive (Phase 6). 0.5.0 added `nullnotes://` links (Phase 5). 0.4.0 added Phase 1 of cross-product search (see the Null Grid search contract v1). The rest of this file describes the 0.3.1 recovery.

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

## Phase 7: two-way sync with Null Drive

Settings → **Null Drive** → Mode → *Two-way sync*. It uses the same address, token and folder as backup. It follows Drive's published contract, `null-grid/notes-sync-drive-api.md` (server 0.8.2+).

- **Matched by note id, not path.** The key is the Markdown frontmatter `id` / `ids.json` / Drive's `synced_id`. Failing that, it's Drive's stable file id; failing that, the path. A rename on any device, in the app or in Explorer, becomes a Drive move (history kept) and is replayed on the other devices. A `.txt` renamed in Explorer gets its id back by content.
- **Merge** (`syncplan.rs`) is three-way: each side against the base saved after the last sync, so offline edits on any number of devices are handled at the next run.
  - Changed on one side: copied to the other.
  - Changed on both: **both kept**. Drive's version becomes `Title (conflict YYYY-MM-DD).md` with a new note id, on every device, and the local version keeps the name.
  - Edited on one side, deleted on the other: the edit wins.
  - Deleted, unchanged elsewhere: deleted (local → Recycle Bin, Drive → 30-day trash).
  - Two new notes with one name: the one already on Drive keeps it.
- **Never silently overwritten.** Every Drive write is conditional: `If-Match: "<version>"`, or `If-None-Match: *` for new files and conflict copies. A `409 conflict` (or `target_exists` / `404`) means Drive moved on, so the step is skipped and the run re-plans within seconds. Before a local file is replaced or deleted, it is hashed again; if it changed since the scan, Drive's version is saved as a conflict copy instead.
- **Editor saves are guarded too.** The editor sends the text the edit started from. If the file changed on disk meanwhile (sync, git, another editor), the disk version stays, the edit is saved as a conflict copy, and a toast says so. Outside changes reload the editor (`vault-changed` event).
- **Guards.** A Drive folder that is suddenly empty, or a vault that looks empty, deletes nothing. More than 20 deletions that are also over half the notes wait for *Sync now* (a confirm).
- **When it runs.** On enable and on vault open, 8 s after a local change, every 2 min to pick up other devices' changes, and on *Sync now*.
- `.nullnotes/ids.json` isn't synced as a file: both sides' maps are merged. The base and hash cache live in `%LOCALAPPDATA%\dev.nullgrid.notes\backup\sync-<hash>.json`. Switching mode or folder resets the base, so the first run is an add-only merge.
- **Not covered:** end-to-end-encrypted Drive accounts (as before). A *locked* note edited on two devices gets a conflict copy with the same (encrypted) note id. Empty folders are created but not deleted.
- **Try it:** `python3 tools/mock_drive.py 8765 mytoken` now implements the sync contract too. `POST /_edit?path=…` plays "another device".

## Phase 8: end-to-end encrypted Drive accounts (5.0.0)

Backup and two-way sync now work into Null Drive accounts that have end-to-end encryption (Drive 0.8.3+), following Drive's published client protocol, `null-grid/notes-e2ee-drive-api.md`. Before 5.0, such accounts answered `409 e2ee_required`. Plain accounts use exactly the code they used before.

- **Unlock once per computer.** Settings → Null Drive → Test connection detects the encryption (`GET /api/v2/e2ee`) and shows *E2EE passphrase or recovery key* → Unlock. That's the passphrase from Drive's encryption setup, not the Drive login.
  - **The key:** Argon2id with the bundle's parameters (or HKDF for the recovery key) unwraps the 32-byte master key with XChaCha20-Poly1305. Seven HKDF-SHA256 subkeys come from it.
  - **The fingerprint** must match Drive's. The master key is stored in Windows Credential Manager (`drive-e2ee:<server>`; on Linux the `0600` secrets file), pinned to that fingerprint. The passphrase is never stored.
  - **Bundle limits:** a bundle Notes doesn't understand is refused before anything is derived (version, kdf, t 1–20, m 8–1024 MiB, p 1, salt and wrap lengths).
- **Every request** carries `X-NullDrive-E2EE: 1`.
- **Names:** every path segment, and the `?prefix=`, is encrypted on its own: ISO 7816-4 padding to 16 bytes, deterministic SIV-XChaCha20-Poly1305, `~` + base64url. Listed paths are decrypted, and rows that don't decrypt (plaintext leftovers while Drive migrates, other keys) are skipped.
- **Content:** goes through the chunked API, not v1 `PUT`/`GET`.
  - Pieces of up to 4 MiB are each sealed `0x01 ‖ SIV(…)`, named by the SHA-256 of the sealed bytes. Then come `POST /api/v2/chunks/check`, `PUT /api/v2/chunks/{hash}` for the missing ones, and the commit `PUT /api/v2/files/{path}` with the plaintext size and the keyed file hash `HMAC(file-hash, SHA-256(plaintext))`.
  - Downloads are manifest plus chunks, checked against size and hash before anything is written.
  - No `mtime` is sent: it would be plaintext metadata, and Notes detects changes by hash and version.
- **Conflicts:** versions, `If-Match` / `If-None-Match: *`, keep-both conflicts, moves and Drive's trash work exactly as in Phase 7.
- **Hashes:** Drive's `sha256` is keyed on these accounts, so local hashes are mapped through the same key before they're compared.
- **No `synced_id`.** Drive can't read frontmatter, so Notes keeps a map from Drive file id to note id in its sync state. A Markdown note's id comes from that map, from the same content at the same path, or from its frontmatter once downloaded (that download is reused by the run).
- **Safety:**
  - A changed account fingerprint stops syncing until the account is unlocked again.
  - A chunk or manifest that doesn't decrypt or verify is reported and nothing is overwritten.
  - Names Windows can't hold (`CON.md`, `a:b`…) are refused before upload, since Drive can't check names it can't read.
  - Switching to or from encryption, or to a new key, resets the sync base, so the first run is an add-only merge.
- **Tests:**
  - **Vectors:** `testdata/e2ee/vectors.json` and `notes-vector.json` (Drive's vectors) are pinned in `e2ee.rs`.
  - **End to end:** `e2ee_tests.rs` runs two devices against `e2ee_mock.rs`, an encrypted Drive that has no key, like the real one. It covers create, edit, conflict, move, delete, a racing write, backup mode, unlock and the fingerprint pin, and checks that no name, note text or note id reaches the server.
  - **Interop:** `tools/e2ee-reference/nulldrive_e2ee.py` is Drive's independent reference client. Against `tools/mock_drive_e2ee.py` it reads everything the app writes, and the app reads what it writes.
- **Try it:** `python3 tools/mock_drive_e2ee.py 8766 mytoken`, then use `http://127.0.0.1:8766` with passphrase `vector passphrase 123`.

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
