# HANDOFF: Null Notes in end-to-end encrypted Drive accounts, Notes side (Phase 8)

**Status:** done, in **Null Notes 5.0.0**. Backup and two-way sync work into Null Drive accounts with end-to-end encryption (Drive 0.8.3+). These accounts used to answer `409 e2ee_required`. The protocol is Drive's `null-grid/notes-e2ee-drive-api.md`; nothing was needed from the Drive side.

**Done-when, as asked:**
- ✅ Tests reproduce both vector files exactly: `null-grid/notes-e2ee-vectors.json` and `null-grid/notes-e2ee-notes-vector.json`.
- ✅ A sync test against an in-memory encrypted Drive covers create, edit, conflict, move and delete.
- ✅ Plain accounts use their old code path. All 34 earlier tests pass unchanged.
- ✅ This handoff.

**Beyond that:**
- The real app was run against an encrypted Drive stand-in, through the real Settings screen.
- Drive's own reference client (`notes-e2ee-reference-client.py`) read every file Notes wrote, and Notes read what it wrote.

| Asked | Where |
|---|---|
| 1. `GET /api/v2/e2ee` on connect; unlock by passphrase or recovery key; verify the fingerprint; master key in the keychain; pin the fingerprint | `backup.rs`: `Drive::e2ee_status`, `Backup::unlock`, `secrets::{get,set,delete}_key`, `open_drive`. UI: Settings → Null Drive |
| 2. `X-NullDrive-E2EE: 1` on every request | `Drive::req` adds it whenever the account is encrypted |
| 3. Encrypt every path and `?prefix=` per name; decrypt listed paths; skip rows that don't decrypt | `Drive::wire` / `wire_url` / `list_rows`. Skipped rows are counted and reported |
| 4. Chunked upload (check, upload, commit) and download (manifest + chunks) | `Drive::commit_encrypted`, `Drive::download_encrypted` |
| 5. No `synced_id`: match by Drive file id and decrypted path, with a local file id → note id map | `sync.rs`: `fill_note_ids`, `SyncState.note_ids` |

## First thing to do (owner)

```bash
cd src-tauri && cargo test --lib                    # 47 tests: 34 from before, 13 new
python3 ../tools/e2ee-reference/nulldrive_e2ee.py vectors   # Drive's reference client checks the same vector copies
```

**To try it in the app without a Drive server:**
1. Run `python3 tools/mock_drive_e2ee.py 8766 mytoken`.
2. In Settings → Null Drive, set the address `http://127.0.0.1:8766`, token `mytoken`, then Test connection.
3. Unlock with `vector passphrase 123` or the recovery key `B9D5-MPJT-B9D5-MPJT-B9D5-MPJT-B9D5-MPJT`.

**Against a real Drive 0.8.3:**
1. Turn on encryption in the Drive app.
2. Make an Everything-scope API token.
3. Do the same steps in Settings, with your real E2EE passphrase.

## What was built

### Crypto (`src-tauri/src/e2ee.rs`, new)
- **Primitives:** HKDF-SHA256, written out with `hmac` + `sha2`. XChaCha20-Poly1305 from `chacha20poly1305`; Argon2id v1.3 from `argon2`. Both crates were already used for locked notes.
- **Keys:**
  - `Keys::from_master` derives the 7 subkeys: `name-enc`, `name-siv`, `chunk-enc`, `chunk-siv`, `file-hash`, `auth`, `fingerprint`.
  - The fingerprint is `hex(subkey[0:8])`.
  - Keys are zeroed on drop.
- **Names:**
  - ISO 7816-4 padding to 16, SIV seal under AD `nulldrive/v1/name`, written `~` + base64url without padding.
  - On decrypt: strip the zeros, require `0x80`, UTF-8, no `/`.
  - 1–255 bytes per name.
- **Content:**
  - `seal_chunk` is `0x01 ‖ SIV(…)` under AD `nulldrive/v1/chunk`. `open_chunk` refuses other versions and tampering.
  - `seal_content` splits into 4 MiB pieces; an empty file has no chunks.
  - `file_hash` / `tag_sha` = `HMAC(file-hash, SHA-256(plaintext))`.
- **Bundle:**
  - `Bundle::check` refuses what Notes doesn't understand before deriving anything: version 1, argon2id, t 1–20, m 8192–1048576 KiB, p 1, 16-byte salts, 72-byte wraps.
  - `unlock` works by passphrase (Argon2id → unwrap, AD `nulldrive/v1/wrap/passphrase`) or by recovery key (Crockford base32, lenient about case, dashes, spaces and O/I/L; then HKDF `nulldrive/v1/recovery-kek` → unwrap, AD `nulldrive/v1/wrap/recovery`). It then checks the fingerprint against the bundle.
  - `guess_secret` decides which the user typed.
- **Names Windows can't hold:** `unsafe_name` applies NOTES-BACKUP's rules (`<>:"\|?*`, control characters, trailing dot or space, CON/PRN/AUX/NUL/COM1-9/LPT1-9). Drive can't check names it can't read.

### Drive client (`backup.rs`)
- **`Drive.keys: Option<Arc<Keys>>`.** With a key: the E2EE header on every request; paths encrypted for folders, listings, deletes and moves; content through v2.
- **`Precond`:** `Any`, `Absent` or `Version(v)`, mapped to `If-Match: *`, `If-None-Match: *` and `If-Match: "v"`.
- **`commit_encrypted`:**
  1. chunk check;
  2. `PUT /api/v2/chunks/{hash}` for the missing ones;
  3. the commit, `{size, sha256, chunks}`.

  `422 missing_chunks` uploads the listed chunks and commits once more. **No `mtime` is sent:** it would be plaintext metadata, and Notes detects changes by hash and version.
- **`download_encrypted`:** the manifest, then each chunk, checked by SHA-256 and opened. The result must match `size` and the keyed hash. If anything fails to decrypt or verify, the error is `DErr::File` and nothing local is written.
- **`e2ee_status`:** `GET /api/v2/e2ee`. On a 404 (Drive before 0.8) it falls back to `/me`.
- **`open_drive`:** used by every run. `state: off` gives a plain client. Otherwise it needs the stored key (`DErr::E2ee` "unlock it" if missing), and the stored fingerprint must equal both the key's and Drive's (`DErr::E2eeKeyChanged`, which stops syncing).
- **Secrets:**
  - The key is stored in its own entry, `drive-e2ee:<server>`: Windows Credential Manager or the macOS Keychain, or on Linux the `0600` `secrets.json`. It holds `{fingerprint, master}`. The passphrase is never stored.
  - `forget_key` deletes it.
- **New error codes mapped:**
  - `e2ee_client_required`, `e2ee_path_required`, `invalid_manifest`: reported as a Notes bug;
  - `hash_mismatch`: transient;
  - `E2eeKeyChanged`: new.
- **Backup mode (`run_once`):** works on encrypted accounts once unlocked. Drive's `sha256` is compared with `drive.tag(local sha)`, so a second run, or another computer with an empty record, uploads nothing that's unchanged. Rows that don't decrypt are reported.

### Sync engine (`sync.rs`)
- **New `Remote` methods** (defaults keep plain behaviour):
  - `tag(sha)`: identity, or the keyed hash;
  - `has_synced_id()`: false on encrypted accounts;
  - `undecryptable()`: how many rows the last listing skipped.
- **Hash mapping:** `run_sync` passes local hashes through `tag` before planning. The same applies to the re-hash checks before a local overwrite or delete, and to the "changed during upload" check. Local, base and Drive hashes are then all comparable. The base is reset when the key changes (`SyncState.e2ee` = fingerprint).
- **`fill_note_ids`.** A Markdown file on Drive gets its note id from, in order:
  1. `SyncState.note_ids` (Drive file id → note id);
  2. a local file at the same path with the same content;
  3. its frontmatter after a download. That download is the one the run needs anyway, kept in `prefetch` and used by the plan's Download / ConflictCopy steps.

  After each run, every key matched on both sides is recorded in the map, and ids Drive no longer lists are pruned. `.txt`/`.html`/`.canvas` ids come from `.nullnotes/ids.json`, which is merged and uploaded encrypted as before.
- **Unreadable files on Drive:** a Download or ConflictCopy of a file that doesn't decrypt is reported and skipped, and the run continues.

### Phase 7 fix found on the way (affects plain accounts too)
- **When:** a device with no sync history meets a note that differs on both sides at different paths.
- **What happened:** the plan moves Drive's file to the local name, then uploads the local version. The upload expected Drive's version from *before* the move. A move bumps the version, so the upload was refused, and the retry made a second conflict copy.
- **Fix:** `Remote::mv` returns the new version (from `{"version"}` or `moved[0].file.version`), and the executor uses it for later steps in the same run.
- **Test:** `plain_fresh_device_same_scenario_keeps_one_copy`.

### Settings (`src/app.js`)
- Under Null Drive, a section appears only for encrypted accounts:
  - **Locked:** "E2EE passphrase or recovery key" (placeholder "Not your Drive login password") and **Unlock**. Unlocking runs off the UI thread.
  - **Unlocked:** "Unlocked on this computer (key `5280…`)" and **Forget key**.
  - **Keys changed:** an amber note and the unlock field again.
- **Test connection** reports the encryption state. A wrong passphrase says "It's your E2EE passphrase, not your Drive login password."
- **New commands:** `backup_e2ee_info`, `backup_unlock`, `backup_forget_key`.

## How it was tested

**Vectors** (`e2ee.rs` tests, copies under `src-tauri/testdata/e2ee/`, checked by Drive's reference client first):
- `vectors.json`: fingerprint, auth key, 5 names (1-byte, Unicode, 16-byte, 255-byte), 2 paths, 3 chunks (empty, small, 1000 bytes) sealed and opened, 2 file hashes.
- The bundle unlocks by passphrase and by recovery key, including messy input. A wrong passphrase or key, a fake fingerprint, and 9 kinds of malformed bundle are all refused.
- `notes-vector.json`: all 7 subkeys, auth hash, the 3 segments with their synthetic nonces, the encrypted and decrypted path, the 81-byte note's hash, keyed hash, sealed chunk, chunk nonce and chunk hash, and the **commit body exactly**.
- Tampered names and chunks are refused, as are other keys, plaintext names, and unknown chunk versions.

**End to end** (`e2ee_tests.rs` against `e2ee_mock.rs`). The mock is an encrypted Drive with **no key**, like the real one, enforcing the header, `~` segments, chunk hashes, the 41-byte size rule, missing chunks, versions, 428, identical-content no-ops and id-keeping moves. The tests:
- **`e2ee_sync_create_edit_conflict_move_delete`:** two devices.
  - **Create:** md, txt, an empty md, an image.
  - **Edit:** on one side.
  - **Nothing changed:** no writes at all, since keyed hashes compare equal.
  - **Both sides edited offline:** both kept; the original keeps its note id and the copy gets a new one.
  - **Explorer renames of a md and an image:** Drive moves, with the same file id.
  - **Delete:** into Drive's trash.
  - **A write racing another device:** never overwritten.
  - **The server saw no plaintext:** no name, note text, note id, `synced_id`, or v1 whole-file call.
- **`e2ee_fresh_device_matches_notes_by_id_without_synced_id`:** an old copy of a note meets its renamed, edited version. It's matched by note id, both versions are kept, and it settles in one sync. **Mutation-checked:** fails with the id map disabled.
- **`e2ee_rows_that_dont_decrypt_are_skipped_and_never_touched`:** a row under another key is skipped, reported, never downloaded or deleted.
- **`e2ee_refusals_match_drive`:**
  - a plain client is refused (`e2ee_client_required`), and backup returns "unlock it" instead of uploading plaintext;
  - `CON.md` is refused client-side;
  - create-only conflicts, and an identical retry is fine;
  - empty and multi-chunk files round-trip;
  - a damaged chunk is refused.
- **`e2ee_backup_mode_uploads_only_changes`:**
  - **Uploads only changes:** a second run and a fresh record both upload nothing; then an edit and a delete go through.
  - **Empty folders** are created, encrypted.
- **`e2ee_unlock_stores_key_and_pins_fingerprint`:**
  - **Locked:** runs stop with "unlock".
  - **Refused:** the login password or a wrong recovery key, and nothing is stored.
  - **Unlocked** by recovery key and by passphrase; the passphrase is never written to disk.
  - **Fingerprint changed** on Drive: `E2eeKeyChanged`.
  - **Forget key:** the stored key is removed.
- **Mutation checks:**
  - mapping local hashes to Drive's keyed hash disabled → the sync test fails;
  - the id map disabled → the fresh-device test fails;
  - the move-version fix removed → the plain regression test fails.

**Real app (5.0.0 release build) with Drive's reference client:**
- `tools/mock_drive_e2ee.py` is a Python stand-in for an encrypted account, enforcing the same rules.
- Drive's reference client (`tools/e2ee-reference/nulldrive_e2ee.py`, copied from the Project) passed its whole flow against it.
- **In the app** (Settings → Null Drive → Two-way sync → address and token → Test connection):
  1. "This account is end-to-end encrypted: unlock it below".
  2. A wrong passphrase gives the clear error; the right one shows "Unlocked (key `5280d8c8b52fe7f7`)".
  3. Save. The 15-file vault synced; every name on the server is `~…` (the folder is `~iQGZXgbQ…`, as in the vector).
- **Interop:**
  - The reference client listed and decrypted all 15 files, **byte-identical** to the vault, with no `synced_id`.
  - It then edited `Start here.md` and created `Inbox/From laptop.md`. After **Sync now**, the app had both, with the note id intact.

**Regression:** all 34 earlier tests pass unchanged (index, agent, links, lock, backup, sync, plain HTTP sync).

## Known issues / next steps

- **Not yet run against a real Drive 0.8.3 server,** only against two independent stand-ins written from the spec, plus Drive's own reference client. First real run: unlock, sync a test vault, open it in the Drive web app.
- **The master key in Windows Credential Manager** goes through the same keyring code that has stored the token since 0.6. It hasn't been run on real Windows.
- **Drive's file list shows upload time, not edit time,** because `mtime` isn't sent on encrypted accounts. It's a deliberate privacy choice: say if Drive's apps should show edit times, and Notes can send it.
- **Unicode:** names are sent byte-for-byte as stored (Drive doesn't normalize). Two devices that spell a name in NFC and NFD would make two files. This is the same as Drive's own clients.
- **Search:** a Notes vault in an encrypted Drive can't be searched through Drive (search is off there). It's still searchable through Notes' own 127.0.0.1 agent, which is what Null Search prefers anyway.
- **Unlock is per Drive server, not per vault:** several vaults on one account share the key.
- **Turning encryption on, changing the passphrase and migrating** stay with the Drive apps. A new passphrase re-wraps the same key, so Notes keeps working. A new key (new fingerprint) stops sync until unlocked again.
- **Files a Drive migration hasn't encrypted yet** are skipped and reported. They show up once Drive's app re-uploads them encrypted.

## Files

| Path | What |
|---|---|
| `src-tauri/src/e2ee.rs` | new: the crypto + vector tests |
| `src-tauri/src/e2ee_mock.rs`, `e2ee_tests.rs` | new (tests only): encrypted Drive mock, end-to-end tests |
| `src-tauri/src/backup.rs` | encrypted Drive client, key storage, `open_drive`, unlock, keyed-hash backup |
| `src-tauri/src/sync.rs` | `Remote::{tag, has_synced_id, undecryptable}`, keyed hashes, note-id map, move-version fix |
| `src-tauri/src/lib.rs` | commands `backup_e2ee_info`, `backup_unlock`, `backup_forget_key` |
| `src/app.js`, `src/backend.js` | Settings: unlock / unlocked / keys-changed |
| `src-tauri/testdata/e2ee/*.json` | Drive's test vectors (verified copies) |
| `tools/mock_drive_e2ee.py` | encrypted Drive stand-in for trying the app |
| `tools/e2ee-reference/nulldrive_e2ee.py` | Drive's reference client (vectors check; interop) |
| `src-tauri/Cargo.toml` | `hmac = "0.12"`; version 5.0.0 |
| `README.md` | Phase 8 section |

## Draft resume bullet

- Added end-to-end encrypted sync to a desktop notes app (Rust/Tauri): an Argon2id/HKDF key hierarchy, deterministic SIV-XChaCha20-Poly1305 names and chunks, and a chunked upload protocol. It reproduces the server team's test vectors byte for byte, interoperates with their independent Python client, and was proven not to leak plaintext by tests against a keyless mock server.
