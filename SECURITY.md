# Security

## What's protected, and how

| Data | Where it lives | Protection |
|---|---|---|
| Notes | your vault folder | Plain files, by design: protected by your OS account and disk encryption |
| Locked notes (`*.lock`) | your vault folder | XChaCha20-Poly1305 under a data key, wrapped by a key from your passphrase (Argon2id, 64 MiB, t=3). The text is never written to disk unencrypted. No recovery if the passphrase is lost |
| Null Drive API token | Windows Credential Manager / macOS Keychain; on Linux a file readable only by you (`0600`) | Never in the vault or in settings files |
| Null Drive encryption key (E2EE accounts) | same place as the token, pinned to the account's fingerprint | Your E2EE passphrase is never stored. If Drive's fingerprint changes, sync stops until you unlock again |
| Notes on an end-to-end encrypted Drive | Null Drive | Every name and all content are encrypted on your computer (SIV-XChaCha20-Poly1305, keys per Drive's published protocol). Drive sees only sizes, folder shape and times |
| Notes on a plain Drive account | Null Drive | TLS in transit (`https://`), plain on the server. `http://` is only allowed on this computer or your local network |
| Search agent | `127.0.0.1:47821` | Loopback only, a 256-bit pairing token, Host-header check against DNS rebinding, and browser access only from the one Null Search address you set |
| `nullnotes://` links | | Only `open` with a valid note id is accepted. A link can't create, change or delete anything |

## Reporting a vulnerability

Please don't open a public issue. Email **luc.honz@proton.me** with the steps to reproduce, and you'll get an answer within a few days.
