# Null Notes (v0.4.0)

0.4.0 adds Phase 1 of cross-product search (see the Null Grid search contract v1). The rest of this file describes the 0.3.1 recovery.

## Phase 1: search agent

- **Note ids.** `.md` notes get `id: <uuid>` in frontmatter, backfilled on first open with modified times kept. The editor never sees the id: it is stripped on read and restored on save. `.txt`, `.html` and `.canvas` ids live in `.nullnotes/ids.json` and follow in-app renames, moves, deletes and lock/unlock. If two files share an id (a copy made in Explorer), the copy gets a new one.
- **Index.** SQLite FTS5 trigram, one DB per vault in `%LOCALAPPDATA%\\dev.nullgrid.notes\\index\\`, never inside the vault. The indexed columns are normalized (lowercase, letters and digits only), so `libre ai`, `Libre-AI` and `LibreAI` all match. Matching runs on the whole query, then on every word in any order. Frontmatter is searchable but kept out of snippets. HTML is indexed without tags; canvas by card text. Locked notes are never indexed.
- **Watcher.** notify, 600 ms debounce, incremental by mtime and size. It catches changes from the app, Explorer, git and sync tools.
- **Agent.** `127.0.0.1:47821` only, `GET /search?q=&limit=` (default 20, max 100). Requires `X-Null-Token` (401). The Host header must be 127.0.0.1 or localhost (403, blocks DNS rebinding). A request with an Origin header is answered only for the configured Null Search origin (403 otherwise), and the preflight returns `Access-Control-Allow-Private-Network: true`. If no vault is open, it returns `[]`.
- **Config.** `%APPDATA%\\dev.nullgrid.notes\\agent.json` holds `{ token, origin }`. The token is 32 random bytes in hex, made on first run. Settings → Null Search shows, copies and regenerates it and sets the origin.

```
curl -H "X-Null-Token: <token>" "http://127.0.0.1:47821/search?q=libre%20ai"
```

Not done yet: registering the `nullnotes://open?id=` link with Windows, so a result opens the note.

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
