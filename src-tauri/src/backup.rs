//! Opt-in, one-way backup of a vault to Null Drive (Null franchise Phase 6).
//!
//! Follows Drive's recipe (Project: `null-grid/notes-backup-drive-api.md`):
//! `POST /api/v1/folders` for the root, `GET /api/v1/files?prefix=` for what
//! Drive has, `PUT /api/v1/files/{path}` with `If-Match: *` and
//! `X-NullDrive-Mtime` for changes, `DELETE` (to Drive's 30-day trash) for
//! files gone from the vault, `POST /api/v1/files/move` for in-app renames.
//!
//! Local → Drive only. Nothing on Drive is ever copied back into the vault.
//!
//! Change tracking: a per-vault record (app data folder, not the vault) of
//! size + mtime + SHA-256 for every file sent. A file is only re-hashed when
//! its size or mtime changed, and only uploaded when its hash differs from
//! what Drive reports.

use crate::vault::{atomic_write, Vault, META_DIR};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

const KEYRING_SERVICE: &str = "dev.nullgrid.notes";
const TMP_SUFFIX: &str = ".nntmp";
const PROBLEMS_MAX: usize = 50;

/* ======================= config, state, status ======================= */

/// Per-vault settings, stored in the app config folder keyed by vault path.
/// Off by default. The token is NOT here (see `secrets`).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct BackupCfg {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub server: String,
    #[serde(default)]
    pub folder: String,
    /// "backup" (one way, Phase 6) or "sync" (two way, Phase 7)
    #[serde(default)]
    pub mode: String,
}

impl BackupCfg {
    pub fn two_way(&self) -> bool {
        self.mode == "sync"
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct FileRec {
    pub size: u64,
    pub mtime_ms: i64,
    pub sha256: String,
}

/// What the last runs sent, so unchanged files are neither hashed nor uploaded.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct State {
    #[serde(default)]
    pub server: String,
    #[serde(default)]
    pub folder: String,
    #[serde(default)]
    pub files: BTreeMap<String, FileRec>,
    /// In-app renames waiting to be replayed as Drive moves (keeps history).
    #[serde(default)]
    pub pending_moves: Vec<(String, String)>,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct Status {
    pub enabled: bool,
    pub server: String,
    pub folder: String,
    pub default_folder: String,
    pub has_token: bool,
    /// off | idle | waiting | running | error
    pub state: String,
    pub message: String,
    pub last_run: Option<String>,
    pub last_ok: Option<String>,
    pub uploaded: usize,
    pub deleted: usize,
    pub moved: usize,
    pub unchanged: usize,
    pub problems: Vec<String>,
    /// "backup" | "sync"
    pub mode: String,
    pub downloaded: usize,
    /// conflict copies made by the last sync (local paths)
    pub conflicts: Vec<String>,
    /// an encryption key is stored for this server (Phase 8)
    pub has_key: bool,
    /// listing rows that couldn't be decrypted (plaintext leftovers, other keys)
    pub undecryptable: usize,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Report {
    pub uploaded: Vec<String>,
    pub deleted: Vec<String>,
    pub moved: usize,
    pub unchanged: usize,
    pub problems: Vec<String>,
}

/* ======================= paths and small helpers ======================= */

pub fn default_folder(vault_name: &str) -> String {
    format!("Null Notes/{vault_name}")
}

/// "/Null Notes/Work/" -> "Null Notes/Work"; refuses empty, `.` and `..` segments.
pub fn clean_folder(f: &str) -> Result<String, String> {
    let f = f.replace('\\', "/");
    let parts: Vec<&str> = f.split('/').map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
    if parts.is_empty() {
        return Err("Choose a folder on Null Drive for this vault.".into());
    }
    if parts.iter().any(|p| *p == "." || *p == "..") {
        return Err("The Drive folder can't contain . or .. segments.".into());
    }
    Ok(parts.join("/"))
}

/// https:// anywhere; http:// only to this computer or a private network,
/// since the token would otherwise cross the internet unencrypted.
pub fn clean_server(s: &str) -> Result<String, String> {
    let s = s.trim().trim_end_matches('/');
    let lower = s.to_ascii_lowercase();
    let (scheme, rest) = if let Some(r) = lower.strip_prefix("https://") {
        ("https", r)
    } else if let Some(r) = lower.strip_prefix("http://") {
        ("http", r)
    } else {
        return Err("Enter the Null Drive address, like https://drive.example.com".into());
    };
    let host_port = rest.split('/').next().unwrap_or("");
    if host_port.is_empty() || rest.contains(['?', '#', '@', ' ']) {
        return Err("Enter the Null Drive address, like https://drive.example.com".into());
    }
    if scheme == "http" {
        let host = host_port.rsplit_once(':').map(|(h, p)| if p.chars().all(|c| c.is_ascii_digit()) { h } else { host_port }).unwrap_or(host_port);
        let host = host.trim_matches(['[', ']']);
        let private = host == "localhost"
            || host.ends_with(".local")
            || host.ends_with(".lan")
            || host.ends_with(".home.arpa")
            || host.parse::<std::net::IpAddr>().map(|ip| match ip {
                std::net::IpAddr::V4(v) => v.is_loopback() || v.is_private() || v.is_link_local(),
                std::net::IpAddr::V6(v) => v.is_loopback() || (v.segments()[0] & 0xfe00) == 0xfc00,
            }).unwrap_or(false);
        if !private {
            return Err("Use https:// for a Drive server on the internet. Plain http:// is only allowed on this computer or your local network.".into());
        }
    }
    // keep the user's path part (Drive behind a reverse-proxy prefix), drop the case-folding of it
    let path = &s[scheme.len() + 3 + host_port.len()..];
    Ok(format!("{scheme}://{host_port}{path}"))
}

/// Percent-encode one path segment (RFC 3986 unreserved characters kept).
fn enc_seg(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            o.push(b as char);
        } else {
            o.push_str(&format!("%{b:02X}"));
        }
    }
    o
}

pub(crate) fn enc_path(p: &str) -> String {
    p.split('/').map(enc_seg).collect::<Vec<_>>().join("/")
}

fn mtime_ms(md: &fs::Metadata) -> i64 {
    md.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_millis() as i64).unwrap_or(0)
}

pub(crate) fn rfc3339_ms(ms: i64) -> String {
    let t = time::OffsetDateTime::from_unix_timestamp_nanos(ms as i128 * 1_000_000).unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    t.format(&time::format_description::well_known::Rfc3339).unwrap_or_default()
}

pub(crate) fn parse_ms(s: &str) -> Option<i64> {
    let t = time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok()?;
    Some((t.unix_timestamp_nanos() / 1_000_000) as i64)
}

pub(crate) fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339).unwrap_or_default()
}

pub(crate) fn sha256_hex(b: &[u8]) -> String {
    Sha256::digest(b).iter().map(|x| format!("{x:02x}")).collect()
}

pub(crate) fn sha256_file(p: &Path) -> std::io::Result<String> {
    let mut f = fs::File::open(p)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn fnv(s: &str) -> u64 {
    s.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3))
}

/* ======================= what to back up ======================= */

pub struct LocalSet {
    pub files: BTreeMap<String, (PathBuf, u64, i64)>, // rel -> (abs, size, mtime)
    pub dirs: BTreeSet<String>,
}

/// Every note, image and locked note the vault lists, plus the vault data in
/// `.nullnotes/` that a restore needs (ids.json, lock.json, settings…).
/// Never `*.nntmp`.
pub fn local_set(root: &Path) -> Result<LocalSet, String> {
    let v = Vault::open(root.to_path_buf())?;
    let mut files = BTreeMap::new();
    let mut dirs = BTreeSet::new();
    for e in v.list_entries()? {
        if e.dir {
            dirs.insert(e.path);
            continue;
        }
        if e.path.ends_with(TMP_SUFFIX) {
            continue;
        }
        let abs = v.abs(&e.path);
        if let Ok(md) = fs::metadata(&abs) {
            files.insert(e.path, (abs, md.len(), mtime_ms(&md)));
        }
    }
    if let Ok(rd) = fs::read_dir(root.join(META_DIR)) {
        for ent in rd.flatten() {
            let name = ent.file_name().to_string_lossy().to_string();
            if !name.ends_with(".json") || name.ends_with(TMP_SUFFIX) {
                continue;
            }
            if let Ok(md) = ent.metadata() {
                if md.is_file() {
                    files.insert(format!("{META_DIR}/{name}"), (ent.path(), md.len(), mtime_ms(&md)));
                }
            }
        }
        dirs.insert(META_DIR.to_string());
    }
    Ok(LocalSet { files, dirs })
}

/* ======================= Drive client ======================= */

#[derive(Debug, Clone, PartialEq)]
pub enum DErr {
    /// 401: token revoked or wrong
    Unauthorized,
    /// 403 insufficient_scope: a Search-only token
    Scope,
    /// The account is end-to-end encrypted and this device hasn't unlocked it
    /// (or it was just switched to encryption): 409 e2ee_required.
    E2ee,
    /// The account's encryption keys changed since this device unlocked it
    /// (fingerprint differs from the pinned one): stop until unlocked again.
    E2eeKeyChanged,
    /// 400 invalid_path / invalid_mtime, 409 path_collision: this file only
    File(String),
    /// 5xx or network: worth retrying
    Transient(String),
    /// the file changed on Drive since we looked (If-Match didn't match): retry next run
    Precondition(String),
    /// anything else
    Other(String),
}

impl DErr {
    pub fn user_message(&self) -> String {
        match self {
            DErr::Unauthorized => "Null Drive refused the token. It may have been revoked: paste a new one.".into(),
            DErr::Scope => "That token is Search only. Create an API token with the Everything scope in Drive (Settings → Devices).".into(),
            DErr::E2ee => "This Drive account is end-to-end encrypted. Unlock it in Settings → Null Drive with your E2EE passphrase or recovery key.".into(),
            DErr::E2eeKeyChanged => "The encryption keys of this Drive account changed since this computer unlocked it. Syncing stopped: unlock it again in Settings → Null Drive.".into(),
            DErr::File(m) | DErr::Other(m) => m.clone(),
            DErr::Transient(m) => format!("Couldn't reach Null Drive: {m}"),
            DErr::Precondition(m) => format!("{m}: changed on Drive meanwhile, will retry"),
        }
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct RemoteFile {
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub sha256: String,
    pub mtime_ms: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct Me {
    pub name: String,
    pub e2ee: String,
}

pub struct Drive {
    base: String,
    token: String,
    agent: ureq::Agent,
    /// Backoff between retries (shortened in tests).
    pub backoff: Vec<Duration>,
    /// Set on an end-to-end encrypted account (Phase 8): every request then
    /// carries `X-NullDrive-E2EE: 1`, paths are encrypted per segment, and
    /// content goes through the chunked v2 API.
    pub keys: Option<Arc<crate::e2ee::Keys>>,
}

/// Write precondition, as in notes-sync-drive-api.md.
#[derive(Debug, Clone, PartialEq)]
pub enum Precond {
    /// `If-Match: *`: replace whatever is there (backup, ids.json)
    Any,
    /// `If-None-Match: *`: create only
    Absent,
    /// `If-Match: <v>`: replace only if Drive is still at v
    Version(i64),
}

/// One listing row, path already decrypted on an encrypted account.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Row {
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    /// plain account: SHA-256 of the content; encrypted: the keyed file hash
    pub sha256: String,
    pub mtime_ms: Option<i64>,
    pub id: String,
    pub version: i64,
    pub synced_id: Option<String>,
}

/// `GET /api/v2/e2ee`
#[derive(Debug, Clone, Default)]
pub struct E2eeStatus {
    /// off | migrating | on
    pub state: String,
    pub fingerprint: String,
    pub bundle: Option<crate::e2ee::Bundle>,
}

/// The commit body for `PUT /api/v2/files/{path}` (notes-e2ee-drive-api.md §4.2).
pub fn commit_body(k: &crate::e2ee::Keys, data: &[u8], chunks: &[(String, Vec<u8>)], mtime: Option<&str>) -> serde_json::Value {
    let mut b = serde_json::json!({
        "size": data.len(),
        "sha256": k.file_hash(data),
        "chunks": chunks.iter().map(|(h, s)| serde_json::json!({"hash": h, "size": s.len()})).collect::<Vec<_>>(),
    });
    if let Some(m) = mtime {
        b["mtime"] = serde_json::Value::String(m.to_string());
    }
    b
}

impl Drive {
    pub fn new(base: &str, token: &str) -> Result<Drive, String> {
        let tls = native_tls::TlsConnector::new().map_err(|e| format!("TLS isn't available: {e}"))?;
        let agent = ureq::AgentBuilder::new()
            .tls_connector(Arc::new(tls))
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(120))
            .timeout_write(Duration::from_secs(300))
            .user_agent(concat!("NullNotes/", env!("CARGO_PKG_VERSION")))
            .build();
        Ok(Drive {
            base: base.trim_end_matches('/').to_string(),
            token: token.to_string(),
            agent,
            backoff: vec![Duration::from_secs(1), Duration::from_secs(4), Duration::from_secs(15)],
            keys: None,
        })
    }

    pub fn encrypted(&self) -> bool {
        self.keys.is_some()
    }

    /// A plaintext path as Drive sees it: encrypted per segment when the
    /// account is, then URL-encoded per segment.
    pub(crate) fn wire(&self, path: &str) -> Result<String, DErr> {
        match &self.keys {
            None => Ok(path.to_string()),
            Some(k) => k.encrypt_path(path).map_err(|e| DErr::File(format!("{path}: {e}"))),
        }
    }

    pub(crate) fn wire_url(&self, path: &str) -> Result<String, DErr> {
        Ok(enc_path(&self.wire(path)?))
    }

    /// What Drive's `sha256` is for content with this plain SHA-256.
    pub fn tag(&self, plain_sha: &str) -> String {
        match &self.keys {
            None => plain_sha.to_string(),
            Some(k) => k.tag_sha(plain_sha),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    pub(crate) fn req(&self, method: &str, path: &str) -> ureq::Request {
        let r = self.agent.request(method, &self.url(path)).set("Authorization", &format!("Bearer {}", self.token));
        if self.keys.is_some() { r.set("X-NullDrive-E2EE", "1") } else { r }
    }

    pub(crate) fn classify(e: ureq::Error, what: &str) -> DErr {
        match e {
            ureq::Error::Status(code, resp) => {
                let body = resp.into_string().unwrap_or_default();
                let errcode = serde_json::from_str::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|v| v.get("error").and_then(|e| e.as_str().map(String::from).or_else(|| e.get("code").and_then(|c| c.as_str()).map(String::from))))
                    .unwrap_or_default();
                match (code, errcode.as_str()) {
                    (401, _) => DErr::Unauthorized,
                    (403, "insufficient_scope") => DErr::Scope,
                    (409, "e2ee_required") => DErr::E2ee,
                    (409, "e2ee_client_required") | (409, "e2ee_path_required") | (400, "invalid_manifest") => DErr::Other(format!("{what}: Drive refused an encrypted request ({errcode}): a Null Notes bug, please report")),
                    (400, "hash_mismatch") => DErr::Transient(format!("{what}: a chunk arrived damaged")),
                    (400, "invalid_path") => DErr::File(format!("{what}: Null Drive can't store this name")),
                    (400, "invalid_mtime") => DErr::File(format!("{what}: Drive refused its modified time")),
                    (409, "path_collision") => DErr::File(format!("{what}: a file and a folder have the same name on Drive")),
                    // Phase 7 (notes-sync-drive-api.md): the "re-plan" answers
                    (409, "conflict") | (409, "target_exists") | (404, "not_found") => DErr::Precondition(what.to_string()),
                    (428, _) => DErr::Other(format!("{what}: Drive wanted a version check (a Null Notes bug, please report)")),
                    (c, _) if c >= 500 || c == 429 => DErr::Transient(format!("{what}: server error {c}")),
                    (c, e) => DErr::Other(format!("{what}: Drive answered {c}{}", if e.is_empty() { String::new() } else { format!(" ({e})") })),
                }
            }
            ureq::Error::Transport(t) => DErr::Transient(format!("{what}: {t}")),
        }
    }

    /// Retry transient failures with backoff; uploads are idempotent.
    pub(crate) fn retry_with<T>(&self, mut f: impl FnMut() -> Result<T, DErr>) -> Result<T, DErr> {
        let mut attempt = 0;
        loop {
            match f() {
                Err(DErr::Transient(_)) if attempt < self.backoff.len() => {
                    std::thread::sleep(self.backoff[attempt]);
                    attempt += 1;
                }
                r => return r,
            }
        }
    }

    pub(crate) fn retry<T>(&self, what: &str, mut f: impl FnMut() -> Result<T, ureq::Error>) -> Result<T, DErr> {
        self.retry_with(|| f().map_err(|e| Self::classify(e, what)))
    }

    pub fn me(&self) -> Result<Me, DErr> {
        let v: serde_json::Value = self.retry("account", || self.req("GET", "/api/v1/me").call())?.into_json().map_err(|e| DErr::Other(format!("account: {e}")))?;
        let u = v.get("user").unwrap_or(&v);
        let s = |k: &str| u.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        let name = [s("name"), s("username"), s("display_name")].into_iter().find(|x| !x.is_empty()).unwrap_or_default();
        Ok(Me { name, e2ee: if s("e2ee").is_empty() { "off".into() } else { s("e2ee") } })
    }

    pub fn mkdir(&self, path: &str) -> Result<(), DErr> {
        let wire = self.wire(path)?;
        self.retry(path, || self.req("POST", "/api/v1/folders").send_json(serde_json::json!({ "path": wire }))).map(|_| ())
    }

    /// `GET /api/v2/e2ee`. A server without it (before 0.8) is treated as
    /// plain when `/me` says encryption is off.
    pub fn e2ee_status(&self) -> Result<E2eeStatus, DErr> {
        match self.req("GET", "/api/v2/e2ee").call() {
            Ok(r) => {
                let v: serde_json::Value = r.into_json().map_err(|e| DErr::Other(format!("encryption status: {e}")))?;
                let state = v.get("state").and_then(|x| x.as_str()).unwrap_or("off").to_string();
                let bundle = v.get("bundle").filter(|b| !b.is_null()).map(|b| serde_json::from_value(b.clone())).transpose()
                    .map_err(|e| DErr::Other(format!("Null Drive sent an encryption key bundle Null Notes doesn't understand ({e}).")))?;
                Ok(E2eeStatus { fingerprint: v.get("fingerprint").and_then(|x| x.as_str()).unwrap_or("").to_string(), state, bundle })
            }
            Err(ureq::Error::Status(404, _)) => {
                let me = self.me()?;
                Ok(E2eeStatus { state: me.e2ee, ..Default::default() })
            }
            Err(e) => Err(Self::classify(e, "encryption status")),
        }
    }

    /// Every row under `prefix` (not deleted), paths decrypted. Rows that
    /// don't decrypt (plaintext leftovers while Drive migrates, other keys)
    /// are skipped and counted.
    pub fn list_rows(&self, prefix: &str) -> Result<(Vec<Row>, usize), DErr> {
        let url = format!("/api/v1/files?prefix={}", self.wire_url(prefix)?);
        let r = self.retry("listing", || self.req("GET", &url).call())?;
        let v: serde_json::Value = r.into_json().map_err(|e| DErr::Other(format!("listing: {e}")))?;
        let arr = v.get("files").and_then(|f| f.as_array()).cloned().unwrap_or_default();
        let mut skipped = 0;
        let mut out = Vec::new();
        for f in arr.iter().filter(|f| !f.get("deleted").and_then(|d| d.as_bool()).unwrap_or(false)) {
            let Some(raw) = f.get("path").and_then(|p| p.as_str()).map(|p| p.trim_matches('/')) else { continue };
            let path = match &self.keys {
                None => raw.to_string(),
                Some(k) => match k.decrypt_path(raw) {
                    Ok(p) => p,
                    Err(_) => {
                        skipped += 1;
                        continue;
                    }
                },
            };
            out.push(Row {
                path,
                is_dir: f.get("is_dir").and_then(|x| x.as_bool()).unwrap_or(false),
                size: f.get("size").and_then(|x| x.as_u64()).unwrap_or(0),
                sha256: f.get("sha256").and_then(|x| x.as_str()).unwrap_or("").to_ascii_lowercase(),
                mtime_ms: f.get("mtime").and_then(|x| x.as_str()).and_then(parse_ms),
                id: f.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                version: f.get("version").and_then(|x| x.as_i64()).unwrap_or(0),
                synced_id: if self.keys.is_some() { None } else { f.get("synced_id").and_then(|x| x.as_str()).map(|s| s.to_lowercase()) },
            });
        }
        Ok((out, skipped))
    }

    pub fn list(&self, prefix: &str) -> Result<Vec<RemoteFile>, DErr> {
        Ok(self
            .list_rows(prefix)?
            .0
            .into_iter()
            .map(|r| RemoteFile { path: r.path, is_dir: r.is_dir, size: r.size, sha256: r.sha256, mtime_ms: r.mtime_ms })
            .collect())
    }

    /* ---- encrypted content (notes-e2ee-drive-api.md §4.2) ---- */

    /// Chunk check, chunk upload, commit. No mtime is sent: it would be
    /// plaintext metadata, and Notes detects changes by hash and version.
    pub fn commit_encrypted(&self, path: &str, data: &[u8], pre: &Precond) -> Result<(), DErr> {
        let k = self.keys.as_ref().ok_or(DErr::Other("not an encrypted account".into()))?;
        if let Some(why) = crate::e2ee::unsafe_name(path) {
            return Err(DErr::File(format!("{path}: Null Drive can't store this name ({why})")));
        }
        let chunks = k.seal_content(data);
        let body = commit_body(k, data, &chunks, None);
        let url = format!("/api/v2/files/{}", self.wire_url(path)?);
        let upload_missing = |want: &[String]| -> Result<(), DErr> {
            for (h, sealed) in chunks.iter().filter(|(h, _)| want.contains(h)) {
                self.retry(path, || self.req("PUT", &format!("/api/v2/chunks/{h}")).set("Content-Type", "application/octet-stream").send_bytes(sealed))?;
            }
            Ok(())
        };
        if !chunks.is_empty() {
            let hashes: Vec<&String> = chunks.iter().map(|(h, _)| h).collect();
            let r: serde_json::Value = self
                .retry(path, || self.req("POST", "/api/v2/chunks/check").send_json(serde_json::json!({ "hashes": hashes })))?
                .into_json()
                .map_err(|e| DErr::Other(format!("{path}: {e}")))?;
            let missing: Vec<String> = r.get("missing").and_then(|m| m.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
            upload_missing(&missing)?;
        }
        for attempt in 0..2 {
            let res = self.retry_with(|| {
                let mut req = self.req("PUT", &url).set("Content-Type", "application/json");
                req = match pre {
                    Precond::Any => req.set("If-Match", "*"),
                    Precond::Absent => req.set("If-None-Match", "*"),
                    // bare, exactly as notes-e2ee-drive-api.md §4.2 and Drive's reference client send it
                    Precond::Version(v) => req.set("If-Match", &v.to_string()),
                };
                match req.send_json(body.clone()) {
                    Ok(_) => Ok(None),
                    // 422 missing_chunks: upload the listed ones and commit again
                    Err(ureq::Error::Status(422, resp)) => {
                        let v: serde_json::Value = resp.into_json().unwrap_or_default();
                        let m = v.get("missing").or_else(|| v.get("error").and_then(|e| e.get("missing"))).and_then(|m| m.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect::<Vec<_>>()).unwrap_or_default();
                        Ok(Some(m))
                    }
                    Err(e) => Err(Self::classify(e, path)),
                }
            })?;
            match res {
                None => return Ok(()),
                Some(m) if attempt == 0 && !m.is_empty() => upload_missing(&m)?,
                Some(_) => break,
            }
        }
        Err(DErr::Other(format!("{path}: Drive kept asking for chunks that were uploaded")))
    }

    /// Manifest, then each chunk, opened and checked against size and hash.
    pub fn download_encrypted(&self, path: &str) -> Result<Vec<u8>, DErr> {
        let k = self.keys.as_ref().ok_or(DErr::Other("not an encrypted account".into()))?;
        let url = format!("/api/v2/files/{}/manifest", self.wire_url(path)?);
        let m: serde_json::Value = self.retry(path, || self.req("GET", &url).call())?.into_json().map_err(|e| DErr::Transient(format!("{path}: {e}")))?;
        let mut out = Vec::with_capacity(m.get("size").and_then(|x| x.as_u64()).unwrap_or(0) as usize);
        for c in m.get("chunks").and_then(|c| c.as_array()).map(|a| a.as_slice()).unwrap_or(&[]) {
            let h = c.get("hash").and_then(|x| x.as_str()).unwrap_or("");
            let sealed = self.retry_with(|| {
                let r = self.req("GET", &format!("/api/v2/chunks/{h}")).call().map_err(|e| Self::classify(e, path))?;
                let mut buf = Vec::new();
                r.into_reader().read_to_end(&mut buf).map_err(|e| DErr::Transient(format!("{path}: {e}")))?;
                if sha256_hex(&buf) != h {
                    return Err(DErr::Transient(format!("{path}: download was cut short")));
                }
                Ok(buf)
            })?;
            // never written anywhere if it doesn't open: corrupt, or other keys
            out.extend(k.open_chunk(&sealed).map_err(|_| DErr::File(format!("{path}: couldn't be decrypted with this account's key (damaged on Drive?). Nothing was changed here.")))?);
        }
        let size = m.get("size").and_then(|x| x.as_u64()).unwrap_or(u64::MAX);
        let sha = m.get("sha256").and_then(|x| x.as_str()).unwrap_or("");
        if out.len() as u64 != size || k.file_hash(&out) != sha {
            return Err(DErr::File(format!("{path}: the decrypted content doesn't match Drive's manifest. Nothing was changed here.")));
        }
        Ok(out)
    }

    pub fn put(&self, remote: &str, abs: &Path, mtime_ms: i64, sha: &str) -> Result<(), DErr> {
        if self.encrypted() {
            let data = fs::read(abs).map_err(|e| DErr::File(format!("{remote}: couldn't read it ({e})")))?;
            return self.commit_encrypted(remote, &data, &Precond::Any);
        }
        self.retry_with(|| {
            // reopened on every attempt: the body is streamed from disk
            let f = fs::File::open(abs).map_err(|e| DErr::File(format!("{remote}: couldn't read it ({e})")))?;
            // explicit length: no chunked encoding (friendlier to reverse proxies).
            // If the file changes mid-upload, Drive's SHA-256 check refuses it and the next run retries.
            let len = f.metadata().map(|m| m.len()).unwrap_or(0);
            self.req("PUT", &format!("/api/v1/files/{}", enc_path(remote)))
                .set("Content-Length", &len.to_string())
                .set("If-Match", "*")
                .set("X-NullDrive-Mtime", &rfc3339_ms(mtime_ms))
                .set("X-Content-SHA256", sha)
                .set("Content-Type", "application/octet-stream")
                .send(f)
                .map(|_| ())
                .map_err(|e| Self::classify(e, remote))
        })
    }

    pub fn delete(&self, remote: &str) -> Result<(), DErr> {
        let url = format!("/api/v1/files/{}", self.wire_url(remote)?);
        match self.retry(remote, || self.req("DELETE", &url).set("If-Match", "*").call()) {
            Ok(_) => Ok(()),
            Err(DErr::Other(m)) if m.contains("answered 404") => Ok(()), // already gone
            Err(DErr::Precondition(_)) => Ok(()),                         // 404 not_found: already gone
            Err(e) => Err(e),
        }
    }

    pub fn mv(&self, from: &str, to: &str) -> Result<(), DErr> {
        let body = serde_json::json!({ "from": self.wire(from)?, "to": self.wire(to)? });
        self.retry(from, || self.req("POST", "/api/v1/files/move").send_json(body.clone())).map(|_| ())
    }
}

/* ======================= one backup run ======================= */

/// Back up `root` into `folder` on Drive. `Err` means the run stopped
/// (token, encryption, network); per-file problems are in `Report.problems`.
pub fn run_once(root: &Path, folder: &str, drive: &Drive, state: &mut State) -> Result<Report, DErr> {
    let mut rep = Report::default();
    let me = drive.me()?;
    if me.e2ee != "off" && !drive.encrypted() {
        return Err(DErr::E2ee);
    }
    if !root.is_dir() {
        return Err(DErr::Other(format!("The vault folder {} isn't there (unplugged drive?). Nothing was changed on Drive.", root.display())));
    }
    let local = local_set(root).map_err(DErr::Other)?;
    drive.mkdir(folder)?;
    let full = |rel: &str| format!("{folder}/{rel}");
    let rel_of = |remote: &str| remote.strip_prefix(&format!("{folder}/")).map(String::from);

    let (rows, skipped) = drive.list_rows(folder)?;
    let mut remote: Vec<RemoteFile> = rows.into_iter().map(|r| RemoteFile { path: r.path, is_dir: r.is_dir, size: r.size, sha256: r.sha256, mtime_ms: r.mtime_ms }).collect();
    if skipped > 0 {
        rep.problems.push(format!("{skipped} file{} in the Drive folder couldn't be decrypted with this account's key and {} left alone.", if skipped == 1 { "" } else { "s" }, if skipped == 1 { "was" } else { "were" }));
    }

    // 1. Replay renames made in the app as moves, so Drive keeps id and history.
    let moves = std::mem::take(&mut state.pending_moves);
    if !moves.is_empty() {
        let have: HashMap<String, bool> = remote.iter().filter_map(|r| rel_of(&r.path).map(|p| (p, r.is_dir))).collect();
        for (from, to) in moves {
            if have.contains_key(&from) && !have.contains_key(&to) && (local.files.contains_key(&to) || local.dirs.contains(&to)) {
                match drive.mv(&full(&from), &full(&to)) {
                    Ok(()) => {
                        rep.moved += 1;
                        // carry the records along so the moved files aren't re-uploaded
                        let pre = format!("{from}/");
                        let carried: Vec<(String, FileRec)> = state.files.iter().filter(|(p, _)| *p == &from || p.starts_with(&pre)).map(|(p, r)| (p.clone(), r.clone())).collect();
                        for (p, r) in carried {
                            state.files.remove(&p);
                            state.files.insert(format!("{to}{}", &p[from.len()..]), r);
                        }
                    }
                    Err(e @ (DErr::Unauthorized | DErr::Scope | DErr::E2ee | DErr::E2eeKeyChanged | DErr::Transient(_))) => return Err(e),
                    Err(_) => {} // fall back to upload-new + delete-old
                }
            }
        }
        if rep.moved > 0 {
            remote = drive.list(folder)?;
        }
    }

    let rfiles: HashMap<String, RemoteFile> = remote.iter().filter(|r| !r.is_dir).filter_map(|r| rel_of(&r.path).map(|p| (p, r.clone()))).collect();
    let rdirs: BTreeSet<String> = remote.iter().filter(|r| r.is_dir).filter_map(|r| rel_of(&r.path)).collect();

    // 2. Upload what changed.
    for (rel, (abs, size, mtime)) in &local.files {
        let rec = state.files.get(rel);
        let sha = match rec {
            Some(r) if r.size == *size && r.mtime_ms == *mtime => r.sha256.clone(),
            _ => match sha256_file(abs) {
                Ok(h) => h,
                Err(e) => {
                    rep.problems.push(format!("{rel}: couldn't read it ({e})"));
                    continue;
                }
            },
        };
        // on an encrypted account Drive's sha256 is the keyed hash of the plaintext
        let same = rfiles.get(rel).map(|r| r.size == *size && r.sha256 == drive.tag(&sha)).unwrap_or(false);
        if same {
            rep.unchanged += 1;
        } else {
            match drive.put(&full(rel), abs, *mtime, &sha) {
                Ok(()) => rep.uploaded.push(rel.clone()),
                Err(DErr::File(m)) | Err(DErr::Other(m)) => {
                    rep.problems.push(m);
                    state.files.remove(rel);
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        state.files.insert(rel.clone(), FileRec { size: *size, mtime_ms: *mtime, sha256: sha });
    }
    // Empty folders, so the backup has the vault's shape.
    for d in local.dirs.iter().filter(|d| !rdirs.contains(*d)) {
        let has_files = local.files.keys().any(|f| f.starts_with(&format!("{d}/")));
        if !has_files {
            if let Err(e) = drive.mkdir(&full(d)) {
                match e {
                    DErr::File(m) | DErr::Other(m) => rep.problems.push(m),
                    e => return Err(e),
                }
            }
        }
    }

    // 3. Send what's gone to Drive's trash. Guard: an empty or missing vault
    // (unmounted disk, wrong folder) must never empty the backup.
    let gone_files: Vec<&String> = rfiles.keys().filter(|p| !local.files.contains_key(*p)).collect();
    if local.files.is_empty() && !rfiles.is_empty() {
        rep.problems.push("The vault looks empty, so nothing was deleted from Drive. If you really emptied it, delete the backup folder in Drive.".into());
    } else {
        // top-most folders that no longer exist locally: one delete each
        let mut gone_dirs: Vec<&String> = rdirs.iter().filter(|d| !local.dirs.contains(*d) && !local.files.keys().any(|f| f.starts_with(&format!("{d}/")))).collect();
        gone_dirs.sort();
        let mut top: Vec<&String> = Vec::new();
        for d in gone_dirs {
            if !top.iter().any(|t| d.starts_with(&format!("{t}/"))) {
                top.push(d);
            }
        }
        for d in &top {
            match drive.delete(&full(d)) {
                Ok(()) => rep.deleted.push(format!("{d}/")),
                Err(DErr::File(m)) | Err(DErr::Other(m)) => rep.problems.push(m),
                Err(e) => return Err(e),
            }
        }
        for p in gone_files {
            if top.iter().any(|t| p.starts_with(&format!("{t}/"))) {
                state.files.remove(p);
                continue;
            }
            match drive.delete(&full(p)) {
                Ok(()) => {
                    rep.deleted.push(p.clone());
                    state.files.remove(p);
                }
                Err(DErr::File(m)) | Err(DErr::Other(m)) => rep.problems.push(m),
                Err(e) => return Err(e),
            }
        }
    }
    state.files.retain(|p, _| local.files.contains_key(p));
    rep.problems.truncate(PROBLEMS_MAX);
    Ok(rep)
}

/* ======================= token storage ======================= */

pub mod secrets {
    //! The Drive token, and on encrypted accounts the account's master key,
    //! live in Windows Credential Manager (macOS: Keychain). Elsewhere
    //! (Linux), a file only this user can read in the config folder.
    //! Never in backup.json, never in the vault.
    use std::collections::BTreeMap;
    use std::path::Path;

    fn token_account(server: &str) -> String {
        format!("drive-backup:{server}")
    }
    fn key_account(server: &str) -> String {
        format!("drive-e2ee:{server}")
    }

    #[allow(dead_code)]
    fn kr_get(account: &str) -> Result<Option<String>, String> {
        let e = keyring::Entry::new(super::KEYRING_SERVICE, account).map_err(|e| e.to_string())?;
        match e.get_password() {
            Ok(t) => Ok(Some(t)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }
    #[allow(dead_code)]
    fn kr_set(account: &str, value: &str) -> Result<(), String> {
        keyring::Entry::new(super::KEYRING_SERVICE, account).and_then(|e| e.set_password(value)).map_err(|e| e.to_string())
    }
    #[allow(dead_code)]
    fn kr_del(account: &str) -> Result<(), String> {
        match keyring::Entry::new(super::KEYRING_SERVICE, account).and_then(|e| e.delete_credential()) {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }

    fn file(dir: &Path) -> std::path::PathBuf {
        dir.join("secrets.json")
    }
    fn f_load(dir: &Path) -> BTreeMap<String, String> {
        std::fs::read(file(dir)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }
    fn f_save(dir: &Path, m: &BTreeMap<String, String>) -> Result<(), String> {
        let _ = std::fs::create_dir_all(dir);
        let p = file(dir);
        crate::vault::atomic_write(&p, serde_json::to_string(m).unwrap().as_bytes())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    const USE_KEYRING: bool = cfg!(any(windows, target_os = "macos"));

    fn get_raw(dir: &Path, account: &str) -> Option<String> {
        if USE_KEYRING {
            return kr_get(account).ok().flatten();
        }
        f_load(dir).get(account).cloned()
    }
    fn set_raw(dir: &Path, account: &str, value: &str) -> Result<(), String> {
        if USE_KEYRING {
            return kr_set(account, value).map_err(|e| format!("Couldn't save it in {}: {e}", if cfg!(windows) { "Windows Credential Manager" } else { "the Keychain" }));
        }
        let mut m = f_load(dir);
        m.insert(account.to_string(), value.to_string());
        f_save(dir, &m)
    }
    fn delete_raw(dir: &Path, account: &str) -> Result<(), String> {
        if USE_KEYRING {
            return kr_del(account);
        }
        let mut m = f_load(dir);
        if m.remove(account).is_some() {
            f_save(dir, &m)?;
        }
        Ok(())
    }

    pub fn get(dir: &Path, server: &str) -> Option<String> {
        get_raw(dir, &token_account(server))
    }
    pub fn set(dir: &Path, server: &str, token: &str) -> Result<(), String> {
        set_raw(dir, &token_account(server), token)
    }
    pub fn delete(dir: &Path, server: &str) -> Result<(), String> {
        delete_raw(dir, &token_account(server))
    }

    /// An unlocked encryption key: the master key and the fingerprint it
    /// was pinned with. The passphrase is never stored.
    #[derive(serde::Serialize, serde::Deserialize, Clone, PartialEq)]
    pub struct StoredKey {
        pub fingerprint: String,
        pub master: String,
    }
    impl std::fmt::Debug for StoredKey {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "StoredKey({})", self.fingerprint)
        }
    }

    pub fn get_key(dir: &Path, server: &str) -> Option<StoredKey> {
        get_raw(dir, &key_account(server)).and_then(|s| serde_json::from_str(&s).ok())
    }
    pub fn set_key(dir: &Path, server: &str, k: &StoredKey) -> Result<(), String> {
        set_raw(dir, &key_account(server), &serde_json::to_string(k).unwrap())
    }
    pub fn delete_key(dir: &Path, server: &str) -> Result<(), String> {
        delete_raw(dir, &key_account(server))
    }
}

/// Make a Drive client for a run: plain, or with the stored key when the
/// account is encrypted. Checks the pinned fingerprint against Drive's.
pub fn open_drive(secrets_dir: &Path, server: &str, token: &str) -> Result<Drive, DErr> {
    let mut d = Drive::new(server, token).map_err(DErr::Other)?;
    let st = d.e2ee_status()?;
    if st.state == "off" {
        return Ok(d);
    }
    let Some(stored) = secrets::get_key(secrets_dir, server) else { return Err(DErr::E2ee) };
    let keys = crate::e2ee::unhex(&stored.master).and_then(|m| crate::e2ee::Keys::from_master(&m).ok()).ok_or(DErr::E2ee)?;
    let server_fp = if st.fingerprint.is_empty() { st.bundle.as_ref().map(|b| b.fingerprint.clone()).unwrap_or_default() } else { st.fingerprint.clone() };
    if keys.fingerprint() != stored.fingerprint || stored.fingerprint != server_fp {
        return Err(DErr::E2eeKeyChanged);
    }
    d.keys = Some(Arc::new(keys));
    Ok(d)
}

/* ======================= scheduler ======================= */

struct Inner {
    vault: Option<(PathBuf, String)>, // (root, name)
    cfg: BackupCfg,
    status: Status,
    kicked_at: Option<Instant>,
    run_now: bool,
    /// next run may delete many files ("Sync now" pressed)
    force: bool,
    quick_retries: u8,
    retry_at: Option<Instant>,
    retry_delay: Duration,
}

pub struct Backup {
    inner: Mutex<Inner>,
    cv: Condvar,
    cfg_path: PathBuf,
    secrets_dir: PathBuf,
    state_dir: PathBuf,
    notify: Box<dyn Fn(&Status) + Send + Sync>,
    /// Quiet period after the last change before a run starts.
    pub debounce: Duration,
    /// A run at least this often while on (also retries failures).
    pub periodic: Duration,
    /// Two-way sync: look for changes on Drive this often.
    pub periodic_sync: Duration,
    pub debounce_sync: Duration,
}

impl Backup {
    pub fn new(config_dir: &Path, data_dir: &Path, notify: Box<dyn Fn(&Status) + Send + Sync>) -> Arc<Backup> {
        let b = Arc::new(Backup {
            inner: Mutex::new(Inner {
                vault: None,
                cfg: BackupCfg::default(),
                status: Status { state: "off".into(), ..Default::default() },
                kicked_at: None,
                run_now: false,
                force: false,
                quick_retries: 0,
                retry_at: None,
                retry_delay: Duration::from_secs(60),
            }),
            cv: Condvar::new(),
            cfg_path: config_dir.join("backup.json"),
            secrets_dir: config_dir.to_path_buf(),
            state_dir: data_dir.join("backup"),
            notify,
            debounce: Duration::from_secs(20),
            periodic: Duration::from_secs(15 * 60),
            periodic_sync: Duration::from_secs(2 * 60),
            debounce_sync: Duration::from_secs(8),
        });
        let me = Arc::downgrade(&b);
        std::thread::spawn(move || loop {
            let Some(b) = me.upgrade() else { return };
            if !b.wait_for_work() {
                return;
            }
            b.run_now_blocking();
        });
        b
    }

    fn all_cfg(&self) -> BTreeMap<String, BackupCfg> {
        fs::read(&self.cfg_path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    fn save_cfg(&self, root: &Path, cfg: &BackupCfg) -> Result<(), String> {
        let mut all = self.all_cfg();
        all.insert(root.display().to_string(), cfg.clone());
        if let Some(d) = self.cfg_path.parent() {
            let _ = fs::create_dir_all(d);
        }
        atomic_write(&self.cfg_path, serde_json::to_string_pretty(&all).unwrap().as_bytes())
    }

    fn state_path(&self, root: &Path) -> PathBuf {
        self.state_dir.join(format!("{:016x}.json", fnv(&root.display().to_string())))
    }

    fn load_state(&self, root: &Path) -> State {
        fs::read(self.state_path(root)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    fn sync_state_path(&self, root: &Path) -> PathBuf {
        self.state_dir.join(format!("sync-{:016x}.json", fnv(&root.display().to_string())))
    }

    fn load_sync_state(&self, root: &Path) -> crate::sync::SyncState {
        fs::read(self.sync_state_path(root)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    fn save_sync_state(&self, root: &Path, st: &crate::sync::SyncState) {
        let _ = fs::create_dir_all(&self.state_dir);
        let _ = atomic_write(&self.sync_state_path(root), serde_json::to_string(st).unwrap().as_bytes());
    }

    fn save_state(&self, root: &Path, st: &State) {
        let _ = fs::create_dir_all(&self.state_dir);
        let _ = atomic_write(&self.state_path(root), serde_json::to_string(st).unwrap().as_bytes());
    }

    fn refresh_status(&self, g: &mut Inner) {
        let name = g.vault.as_ref().map(|v| v.1.clone()).unwrap_or_default();
        g.status.enabled = g.cfg.enabled && g.vault.is_some();
        g.status.server = g.cfg.server.clone();
        g.status.default_folder = default_folder(&name);
        g.status.folder = if g.cfg.folder.is_empty() { default_folder(&name) } else { g.cfg.folder.clone() };
        g.status.mode = if g.cfg.two_way() { "sync".into() } else { "backup".into() };
        g.status.has_token = !g.cfg.server.is_empty() && secrets::get(&self.secrets_dir, &g.cfg.server).is_some();
        g.status.has_key = !g.cfg.server.is_empty() && secrets::get_key(&self.secrets_dir, &g.cfg.server).is_some();
        if !g.status.enabled {
            g.status.state = "off".into();
        } else if g.status.state == "off" {
            g.status.state = "idle".into();
        }
    }

    fn publish(&self, g: &Inner) {
        (self.notify)(&g.status);
    }

    pub fn status(&self) -> Status {
        let mut g = self.inner.lock().unwrap();
        self.refresh_status(&mut g);
        g.status.clone()
    }

    pub fn set_vault(&self, vault: Option<(PathBuf, String)>) {
        let mut g = self.inner.lock().unwrap();
        g.cfg = vault.as_ref().and_then(|(r, _)| self.all_cfg().get(&r.display().to_string()).cloned()).unwrap_or_default();
        g.vault = vault;
        g.status = Status { state: "off".into(), ..Default::default() };
        g.retry_at = None;
        self.refresh_status(&mut g);
        if g.status.enabled {
            g.run_now = true; // catch up with changes made while the app was closed
            self.cv.notify_all();
        }
        self.publish(&g);
    }

    /// Save settings from the Settings screen. `token: Some("")` removes it.
    pub fn configure(&self, enabled: bool, server: &str, folder: &str, token: Option<&str>, mode: &str) -> Result<Status, String> {
        let mut g = self.inner.lock().unwrap();
        let Some((root, name)) = g.vault.clone() else { return Err("Open a vault first.".into()) };
        let server = if server.trim().is_empty() && !enabled { String::new() } else { clean_server(server)? };
        let folder = if folder.trim().is_empty() { default_folder(&name) } else { clean_folder(folder)? };
        if let Some(t) = token {
            if t.trim().is_empty() {
                if !server.is_empty() {
                    secrets::delete(&self.secrets_dir, &server)?;
                }
            } else {
                secrets::set(&self.secrets_dir, &server, t.trim())?;
            }
        }
        if enabled && secrets::get(&self.secrets_dir, &server).is_none() {
            return Err("Paste a Null Drive API token to turn on backup.".into());
        }
        let mode = if mode == "sync" { "sync" } else { "backup" };
        let cfg = BackupCfg { enabled, server, folder, mode: mode.into() };
        let target_changed = cfg.server != g.cfg.server || cfg.folder != g.cfg.folder;
        let mode_changed = cfg.mode != g.cfg.mode;
        self.save_cfg(&root, &cfg)?;
        g.cfg = cfg;
        if target_changed {
            // New destination: forget what was sent where.
            self.save_state(&root, &State::default());
        }
        if target_changed || mode_changed {
            // The first two-way run then merges both sides (adds only, never deletes).
            self.save_sync_state(&root, &crate::sync::SyncState::default());
        }
        g.status.message.clear();
        g.status.state = if enabled { "idle".into() } else { "off".into() };
        self.refresh_status(&mut g);
        if enabled {
            g.run_now = true;
            self.cv.notify_all();
        }
        self.publish(&g);
        Ok(g.status.clone())
    }

    /// "Test connection" in Settings: account name, encryption, token scope.
    pub fn test(&self, server: &str, token: Option<&str>) -> Result<String, String> {
        let server = clean_server(server)?;
        let token = match token.filter(|t| !t.trim().is_empty()) {
            Some(t) => t.trim().to_string(),
            None => secrets::get(&self.secrets_dir, &server).ok_or("Paste a Null Drive API token first.")?,
        };
        let mut d = Drive::new(&server, &token)?;
        d.backoff = vec![];
        let me = d.me().map_err(|e| e.user_message())?;
        let hello = if me.name.is_empty() { "Connected.".to_string() } else { format!("Connected as {}.", me.name) };
        let st = d.e2ee_status().map_err(|e| e.user_message())?;
        if st.state != "off" {
            // checked with the stored key, if any (a Search-only token fails the listing)
            return match open_drive(&self.secrets_dir, &server, &token) {
                Ok(mut d) => {
                    d.backoff = vec![];
                    d.list("Null Notes").map_err(|e| e.user_message())?;
                    Ok(format!("{hello} End-to-end encrypted, unlocked on this computer (key {}).", st.fingerprint))
                }
                Err(DErr::E2eeKeyChanged) => Err(format!("{hello} {}", DErr::E2eeKeyChanged.user_message())),
                Err(DErr::E2ee) => Ok(format!("{hello} This account is end-to-end encrypted: unlock it below with your E2EE passphrase or recovery key.")),
                Err(e) => Err(e.user_message()),
            };
        }
        // A Search-only token can read /me but not the files API.
        d.list("Null Notes").map_err(|e| e.user_message())?;
        Ok(hello)
    }

    /// Encryption state for Settings: is the account encrypted, and is it
    /// unlocked here with a key matching Drive's fingerprint?
    pub fn e2ee_info(&self, server: &str, token: Option<&str>) -> Result<serde_json::Value, String> {
        let server = clean_server(server)?;
        let token = match token.filter(|t| !t.trim().is_empty()) {
            Some(t) => t.trim().to_string(),
            None => secrets::get(&self.secrets_dir, &server).ok_or("Paste a Null Drive API token first.")?,
        };
        let mut d = Drive::new(&server, &token)?;
        d.backoff = vec![];
        let st = d.e2ee_status().map_err(|e| e.user_message())?;
        let stored = secrets::get_key(&self.secrets_dir, &server);
        let unlocked = st.state != "off" && stored.as_ref().map(|k| k.fingerprint == st.fingerprint).unwrap_or(false);
        let key_changed = st.state != "off" && stored.is_some() && !unlocked;
        Ok(serde_json::json!({ "state": st.state, "fingerprint": st.fingerprint, "unlocked": unlocked, "key_changed": key_changed }))
    }

    /// Unlock with the E2EE passphrase or recovery key: fetch the bundle,
    /// unwrap the master key, check the fingerprint, keep the key (keychain)
    /// pinned to that fingerprint. The passphrase itself is never kept.
    pub fn unlock(&self, server: &str, token: Option<&str>, secret: &str) -> Result<String, String> {
        let server = clean_server(server)?;
        let token = match token.filter(|t| !t.trim().is_empty()) {
            Some(t) => t.trim().to_string(),
            None => secrets::get(&self.secrets_dir, &server).ok_or("Paste a Null Drive API token first.")?,
        };
        let mut d = Drive::new(&server, &token)?;
        d.backoff = vec![];
        let st = d.e2ee_status().map_err(|e| e.user_message())?;
        if st.state == "off" {
            return Err("This Drive account isn't end-to-end encrypted: there's nothing to unlock.".into());
        }
        let bundle = st.bundle.ok_or("Null Drive didn't send the account's key bundle.")?;
        if bundle.fingerprint != st.fingerprint && !st.fingerprint.is_empty() {
            return Err("Null Drive's key bundle doesn't match the account's fingerprint. Nothing was unlocked.".into());
        }
        let secret = secret.trim();
        if secret.is_empty() {
            return Err("Type your E2EE passphrase or recovery key.".into());
        }
        let keys = crate::e2ee::unlock(&bundle, &crate::e2ee::guess_secret(secret)).map_err(|e| match e {
            crate::e2ee::E2eeErr::Decrypt => match crate::e2ee::guess_secret(secret) {
                crate::e2ee::Secret::RecoveryKey(_) => "That recovery key doesn't unlock this account.".to_string(),
                crate::e2ee::Secret::Passphrase(_) => "That passphrase doesn't unlock this account. It's your E2EE passphrase, not your Drive login password.".to_string(),
            },
            crate::e2ee::E2eeErr::Fingerprint => "The unlocked key doesn't match the account's fingerprint. Nothing was saved.".to_string(),
            crate::e2ee::E2eeErr::Format(m) => m,
        })?;
        secrets::set_key(&self.secrets_dir, &server, &secrets::StoredKey { fingerprint: keys.fingerprint(), master: keys.master_hex() })?;
        let mut g = self.inner.lock().unwrap();
        if g.cfg.enabled && g.cfg.server == server {
            g.run_now = true;
            self.cv.notify_all();
        }
        Ok(format!("Unlocked. Key {} is saved on this computer; your passphrase isn't.", keys.fingerprint()))
    }

    /// Forget the stored key for this server (Settings → "Forget key").
    pub fn forget_key(&self, server: &str) -> Result<(), String> {
        secrets::delete_key(&self.secrets_dir, &clean_server(server)?)
    }

    /// A file changed in the vault (watcher). Runs after a quiet period.
    pub fn kick(&self) {
        let mut g = self.inner.lock().unwrap();
        if g.cfg.enabled && g.vault.is_some() {
            g.kicked_at = Some(Instant::now());
            if g.status.state == "idle" {
                g.status.state = "waiting".into();
                self.publish(&g);
            }
            self.cv.notify_all();
        }
    }

    pub fn back_up_now(&self) {
        let mut g = self.inner.lock().unwrap();
        g.run_now = true;
        g.force = true; // the user asked: large deletions held back last time may go ahead
        self.cv.notify_all();
    }

    /// The app renamed or moved something: replay it as a Drive move later.
    pub fn renamed(&self, from: &str, to: &str) {
        let g = self.inner.lock().unwrap();
        if !g.cfg.enabled || g.cfg.two_way() {
            return; // two-way sync recognises renames by note id itself
        }
        if let Some((root, _)) = g.vault.clone() {
            let mut st = self.load_state(&root);
            st.pending_moves.push((from.to_string(), to.to_string()));
            self.save_state(&root, &st);
        }
    }

    /// Block until a run is due. Returns false when the backup is dropped.
    fn wait_for_work(&self) -> bool {
        let mut g = self.inner.lock().unwrap();
        loop {
            let on = g.cfg.enabled && g.vault.is_some();
            let now = Instant::now();
            let (debounce, periodic) = if g.cfg.two_way() { (self.debounce_sync, self.periodic_sync) } else { (self.debounce, self.periodic) };
            if on {
                if g.run_now {
                    return true;
                }
                if let Some(k) = g.kicked_at {
                    if now.duration_since(k) >= debounce {
                        return true;
                    }
                }
                if let Some(r) = g.retry_at {
                    if now >= r {
                        return true;
                    }
                }
            }
            let mut wait = periodic;
            if let Some(k) = g.kicked_at {
                wait = wait.min((k + debounce).saturating_duration_since(now));
            }
            if let Some(r) = g.retry_at {
                wait = wait.min(r.saturating_duration_since(now));
            }
            let (ng, to) = self.cv.wait_timeout(g, wait.max(Duration::from_millis(50))).unwrap();
            g = ng;
            if to.timed_out() && on && g.kicked_at.is_none() && g.retry_at.is_none() && !g.run_now {
                // periodic safety run (catches changes the watcher missed)
                g.run_now = true;
            }
        }
    }

    pub fn run_now_blocking(&self) {
        let (root, cfg, force) = {
            let mut g = self.inner.lock().unwrap();
            g.run_now = false;
            g.kicked_at = None;
            g.retry_at = None;
            let force = std::mem::take(&mut g.force);
            let Some((root, _)) = g.vault.clone() else { return };
            if !g.cfg.enabled {
                return;
            }
            self.refresh_status(&mut g);
            g.status.state = "running".into();
            g.status.message = if g.cfg.two_way() { "Syncing…".into() } else { "Backing up…".into() };
            self.publish(&g);
            (root, g.cfg.clone(), force)
        };
        if cfg.two_way() {
            return self.sync_now_blocking(root, cfg, force);
        }
        let folder = if cfg.folder.is_empty() { default_folder(&root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()) } else { cfg.folder.clone() };
        let result = match secrets::get(&self.secrets_dir, &cfg.server) {
            None => Err(DErr::Other("Paste a Null Drive API token in Settings → Back up to Null Drive.".into())),
            Some(token) => match open_drive(&self.secrets_dir, &cfg.server, &token) {
                Err(e) => Err(e),
                Ok(drive) => {
                    let mut st = self.load_state(&root);
                    if st.server != cfg.server || st.folder != folder {
                        st = State { server: cfg.server.clone(), folder: folder.clone(), ..Default::default() };
                    }
                    let r = run_once(&root, &folder, &drive, &mut st);
                    self.save_state(&root, &st);
                    r
                }
            },
        };
        let mut g = self.inner.lock().unwrap();
        if g.vault.as_ref().map(|v| v.0 != root).unwrap_or(true) {
            return; // vault switched mid-run
        }
        g.status.last_run = Some(now_rfc3339());
        match result {
            Ok(rep) => {
                g.status.state = "idle".into();
                g.status.last_ok = g.status.last_run.clone();
                g.status.uploaded = rep.uploaded.len();
                g.status.deleted = rep.deleted.len();
                g.status.moved = rep.moved;
                g.status.unchanged = rep.unchanged;
                g.status.downloaded = 0;
                g.status.conflicts.clear();
                g.status.message = if rep.problems.is_empty() { "Backed up.".into() } else { format!("Backed up, {} problem{}.", rep.problems.len(), if rep.problems.len() == 1 { "" } else { "s" }) };
                g.status.problems = rep.problems;
                g.retry_delay = Duration::from_secs(60);
            }
            Err(e) => {
                g.status.state = "error".into();
                g.status.message = e.user_message();
                // Network trouble retries on its own; token/encryption problems wait for the user.
                if matches!(e, DErr::Transient(_) | DErr::Other(_)) {
                    g.retry_at = Some(Instant::now() + g.retry_delay);
                    g.retry_delay = (g.retry_delay * 2).min(Duration::from_secs(30 * 60));
                }
            }
        }
        self.refresh_status(&mut g);
        self.publish(&g);
    }
}

impl Backup {
    /// One two-way sync run (Phase 7), with quick re-runs when Drive or the
    /// disk changed under it.
    fn sync_now_blocking(&self, root: PathBuf, cfg: BackupCfg, force: bool) {
        let folder = if cfg.folder.is_empty() { default_folder(&root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()) } else { cfg.folder.clone() };
        let result = match secrets::get(&self.secrets_dir, &cfg.server) {
            None => Err(DErr::Other("Paste a Null Drive API token in Settings → Null Drive.".into())),
            Some(token) => match open_drive(&self.secrets_dir, &cfg.server, &token) {
                Err(e) => Err(e),
                Ok(drive) => (|| {
                    drive.mkdir(&folder)?;
                    let mut st = self.load_sync_state(&root);
                    // hashes in the base are keyed on an encrypted account: a
                    // different key (or none) means starting over with an add-only merge
                    let fp = drive.keys.as_ref().map(|k| k.fingerprint()).unwrap_or_default();
                    if st.server != cfg.server || st.folder != folder || st.e2ee != fp {
                        st = crate::sync::SyncState { server: cfg.server.clone(), folder: folder.clone(), e2ee: fp, ..Default::default() };
                    }
                    let remote = crate::sync::DriveRemote::new(&drive, folder.clone());
                    let r = crate::sync::run_sync(&root, &remote, &mut st, force);
                    self.save_sync_state(&root, &st);
                    r.map(|rep| (rep, remote.skipped.get()))
                })(),
            },
        };
        let mut g = self.inner.lock().unwrap();
        if g.vault.as_ref().map(|v| v.0 != root).unwrap_or(true) {
            return;
        }
        g.status.last_run = Some(now_rfc3339());
        match result {
            Ok((rep, skipped)) => {
                g.status.undecryptable = skipped;
                g.status.state = "idle".into();
                g.status.last_ok = g.status.last_run.clone();
                g.status.uploaded = rep.uploaded.len();
                g.status.downloaded = rep.downloaded.len();
                g.status.deleted = rep.deleted_local.len() + rep.deleted_remote.len();
                g.status.moved = rep.moved.len();
                g.status.unchanged = 0;
                g.status.conflicts = rep.conflicts.clone();
                g.status.message = match (rep.conflicts.len(), rep.problems.len()) {
                    (0, 0) => "Synced.".into(),
                    (c, 0) => format!("Synced. {c} note{} changed in both places: both versions kept.", if c == 1 { "" } else { "s" }),
                    (0, p) => format!("Synced, {p} problem{}.", if p == 1 { "" } else { "s" }),
                    (c, p) => format!("Synced. {c} kept-both conflict{}, {p} problem{}.", if c == 1 { "" } else { "s" }, if p == 1 { "" } else { "s" }),
                };
                g.status.problems = rep.problems;
                g.retry_delay = Duration::from_secs(60);
                if rep.retry && g.quick_retries < 3 {
                    // something changed under us: look again in a moment
                    g.quick_retries += 1;
                    g.retry_at = Some(Instant::now() + Duration::from_secs(3));
                } else {
                    g.quick_retries = 0;
                }
            }
            Err(e) => {
                g.status.state = "error".into();
                g.status.message = e.user_message();
                if matches!(e, DErr::Transient(_) | DErr::Other(_) | DErr::Precondition(_)) {
                    g.retry_at = Some(Instant::now() + g.retry_delay);
                    g.retry_delay = (g.retry_delay * 2).min(Duration::from_secs(30 * 60));
                }
            }
        }
        self.refresh_status(&mut g);
        self.publish(&g);
    }
}

#[cfg(test)]
pub mod mock {
    //! In-process stand-in for Null Drive's backup API (server 0.8+), with
    //! fault injection. Behaviour per `null-grid/notes-backup-drive-api.md`.
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    use tiny_http::{Header, Response, Server};

    #[derive(Clone, Debug)]
    pub struct F {
        pub data: Vec<u8>,
        pub sha: String,
        pub mtime: Option<String>,
        pub versions: usize,
    }

    #[derive(Default)]
    pub struct S {
        pub files: BTreeMap<String, F>,
        pub dirs: std::collections::BTreeSet<String>,
        pub trash: Vec<String>,
        pub puts: usize,
        pub moves: Vec<(String, String)>,
        pub token: String,
        pub e2ee: bool,
        pub search_only: bool,
        pub fail_next: usize, // answer 503 this many times
        pub log: Vec<String>,
    }

    pub struct Mock {
        pub url: String,
        pub s: Arc<Mutex<S>>,
    }

    fn dec(p: &str) -> String {
        let b = p.as_bytes();
        let mut o = Vec::new();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'%' && i + 2 < b.len() {
                o.push(u8::from_str_radix(&p[i + 1..i + 3], 16).unwrap());
                i += 3;
            } else {
                o.push(b[i]);
                i += 1;
            }
        }
        String::from_utf8(o).unwrap()
    }

    fn bad_name(p: &str) -> bool {
        p.split('/').any(|seg| {
            let stem = seg.split('.').next().unwrap_or("").to_ascii_uppercase();
            seg.contains(['<', '>', ':', '"', '|', '?', '*']) || seg.ends_with('.') || seg.ends_with(' ') || ["CON", "NUL", "PRN", "AUX"].contains(&stem.as_str())
        })
    }

    pub fn start(token: &str) -> Mock {
        let server = Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", server.server_addr().to_ip().unwrap().port());
        let s = Arc::new(Mutex::new(S { token: token.into(), ..Default::default() }));
        let st = s.clone();
        std::thread::spawn(move || {
            for mut req in server.incoming_requests() {
                let mut g = st.lock().unwrap();
                let j = |code: u16, v: serde_json::Value| Response::from_string(v.to_string()).with_status_code(code).with_header(Header::from_bytes("Content-Type", "application/json").unwrap());
                let auth = req.headers().iter().find(|h| h.field.equiv("Authorization")).map(|h| h.value.to_string()).unwrap_or_default();
                let method = req.method().to_string();
                let url = req.url().to_string();
                g.log.push(format!("{method} {url}"));
                if auth != format!("Bearer {}", g.token) {
                    let _ = req.respond(j(401, serde_json::json!({"error":"unauthorized"})));
                    continue;
                }
                if g.fail_next > 0 {
                    g.fail_next -= 1;
                    let _ = req.respond(j(503, serde_json::json!({"error":"unavailable"})));
                    continue;
                }
                let (path, query) = url.split_once('?').map(|(a, b)| (a.to_string(), b.to_string())).unwrap_or((url.clone(), String::new()));
                if path == "/api/v1/me" {
                    let _ = req.respond(j(200, serde_json::json!({"user":{"name":"luke","e2ee": if g.e2ee {"on"} else {"off"}}})));
                    continue;
                }
                if g.search_only {
                    let _ = req.respond(j(403, serde_json::json!({"error":"insufficient_scope"})));
                    continue;
                }
                let mut body = Vec::new();
                let _ = req.as_reader().read_to_end(&mut body);
                let hdr = |n: &str| req.headers().iter().find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(n)).map(|h| h.value.to_string());
                match (method.as_str(), path.as_str()) {
                    ("POST", "/api/v1/folders") => {
                        let p: serde_json::Value = serde_json::from_slice(&body).unwrap();
                        let p = p["path"].as_str().unwrap().to_string();
                        if g.e2ee { let _ = req.respond(j(409, serde_json::json!({"error":"e2ee_required"}))); continue; }
                        let new = g.dirs.insert(p.clone());
                        let _ = req.respond(j(if new { 201 } else { 200 }, serde_json::json!({"path": p})));
                    }
                    ("GET", "/api/v1/files") => {
                        let prefix = dec(query.split('&').find_map(|kv| kv.strip_prefix("prefix=")).unwrap_or(""));
                        let under = |p: &str| p.starts_with(&format!("{prefix}/"));
                        let mut out: Vec<serde_json::Value> = g.files.iter().filter(|(p, _)| under(p)).map(|(p, f)| serde_json::json!({"path": p, "is_dir": false, "size": f.data.len(), "sha256": f.sha, "mtime": f.mtime})).collect();
                        out.extend(g.dirs.iter().filter(|d| under(d)).map(|d| serde_json::json!({"path": d, "is_dir": true, "size": 0, "sha256": ""})));
                        let _ = req.respond(j(200, serde_json::json!({"version": 1, "files": out})));
                    }
                    ("PUT", p) if p.starts_with("/api/v1/files/") => {
                        let rp = dec(&p["/api/v1/files/".len()..]);
                        if g.e2ee { let _ = req.respond(j(409, serde_json::json!({"error":"e2ee_required"}))); continue; }
                        if hdr("If-Match").as_deref() != Some("*") { let _ = req.respond(j(428, serde_json::json!({"error":"precondition_required"}))); continue; }
                        if bad_name(&rp) { let _ = req.respond(j(400, serde_json::json!({"error":"invalid_path"}))); continue; }
                        if g.dirs.contains(&rp) { let _ = req.respond(j(409, serde_json::json!({"error":"path_collision"}))); continue; }
                        let sha = crate::backup::tests::sha_hex(&body);
                        if let Some(claimed) = hdr("X-Content-SHA256") { assert_eq!(claimed, sha, "client sent a wrong hash for {rp}"); }
                        let mtime = hdr("X-NullDrive-Mtime");
                        assert!(mtime.as_deref().map(|m| crate::backup::parse_ms(m).is_some()).unwrap_or(false), "bad mtime");
                        let mut parts: Vec<&str> = rp.split('/').collect();
                        parts.pop();
                        for i in 1..=parts.len() { let d = parts[..i].join("/"); g.dirs.insert(d); }
                        let versions = g.files.get(&rp).map(|f| f.versions + 1).unwrap_or(1);
                        let existed = g.files.contains_key(&rp);
                        g.files.insert(rp, F { data: body, sha, mtime, versions });
                        g.puts += 1;
                        let _ = req.respond(j(if existed { 200 } else { 201 }, serde_json::json!({})));
                    }
                    ("DELETE", p) if p.starts_with("/api/v1/files/") => {
                        let rp = dec(&p["/api/v1/files/".len()..]);
                        assert_eq!(hdr("If-Match").as_deref(), Some("*"));
                        let pre = format!("{rp}/");
                        let gone: Vec<String> = g.files.keys().filter(|k| *k == &rp || k.starts_with(&pre)).cloned().collect();
                        let had_dir = g.dirs.remove(&rp);
                        g.dirs.retain(|d| !d.starts_with(&pre));
                        if gone.is_empty() && !had_dir { let _ = req.respond(j(404, serde_json::json!({"error":"not_found"}))); continue; }
                        for k in &gone { g.files.remove(k); }
                        g.trash.extend(gone.clone());
                        if had_dir { g.trash.push(format!("{rp}/")); }
                        let _ = req.respond(j(200, serde_json::json!({"deleted": gone})));
                    }
                    ("POST", "/api/v1/files/move") => {
                        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
                        let (from, to) = (v["from"].as_str().unwrap().to_string(), v["to"].as_str().unwrap().to_string());
                        let pre = format!("{from}/");
                        let moving: Vec<String> = g.files.keys().filter(|k| *k == &from || k.starts_with(&pre)).cloned().collect();
                        if moving.is_empty() && !g.dirs.contains(&from) { let _ = req.respond(j(404, serde_json::json!({"error":"not_found"}))); continue; }
                        for k in moving { let f = g.files.remove(&k).unwrap(); g.files.insert(format!("{to}{}", &k[from.len()..]), f); }
                        let ds: Vec<String> = g.dirs.iter().filter(|d| *d == &from || d.starts_with(&pre)).cloned().collect();
                        for d in ds { g.dirs.remove(&d); g.dirs.insert(format!("{to}{}", &d[from.len()..])); }
                        let mut parts: Vec<&str> = to.split('/').collect();
                        parts.pop();
                        for i in 1..=parts.len() { let d = parts[..i].join("/"); g.dirs.insert(d); }
                        g.moves.push((from, to));
                        let _ = req.respond(j(200, serde_json::json!({})));
                    }
                    _ => { let _ = req.respond(j(404, serde_json::json!({"error":"not_found"}))); }
                }
            }
        });
        Mock { url, s }
    }
}

#[cfg(test)]
mod tests {
    use super::*;


    pub fn sha_hex(b: &[u8]) -> String {
        Sha256::digest(b).iter().map(|x| format!("{x:02x}")).collect()
    }

    fn vault() -> PathBuf {
        let d = std::env::temp_dir().join(format!("nn-bk-{}-{}", std::process::id(), time::OffsetDateTime::now_utc().unix_timestamp_nanos()));
        fs::create_dir_all(d.join("Projects")).unwrap();
        fs::create_dir_all(d.join("Empty")).unwrap();
        fs::create_dir_all(d.join(META_DIR)).unwrap();
        fs::write(d.join("Welcome.md"), "---\nid: 11111111-1111-4111-8111-111111111111\n---\n# hi").unwrap();
        fs::write(d.join("Projects/Plan.md"), "# plan").unwrap();
        fs::write(d.join("Projects/todo.txt"), "[ ] x").unwrap();
        fs::write(d.join("Projects/pic.png"), b"\x89PNG fake").unwrap();
        fs::write(d.join("secret.md.lock"), b"NNLOCK1\nciphertext").unwrap();
        fs::write(d.join("Welcome.md.nntmp"), "half").unwrap();
        fs::write(d.join("ignored.pdf"), "x").unwrap();
        fs::write(d.join(META_DIR).join("ids.json"), "{}").unwrap();
        fs::write(d.join(META_DIR).join("lock.json"), "{}").unwrap();
        fs::write(d.join(META_DIR).join("settings.json"), "{}").unwrap();
        d
    }

    fn drive(m: &mock::Mock, token: &str) -> Drive {
        let mut d = Drive::new(&m.url, token).unwrap();
        d.backoff = vec![Duration::from_millis(10); 3];
        d
    }

    #[test]
    fn full_backup_then_only_changes_then_deletes() {
        let m = mock::start("tok");
        let root = vault();
        let d = drive(&m, "tok");
        let mut st = State::default();
        let f = "Null Notes/Work";

        let r = run_once(&root, f, &d, &mut st).unwrap();
        {
            let g = m.s.lock().unwrap();
            let names: Vec<&str> = g.files.keys().map(|k| k.as_str()).collect();
            assert_eq!(names, vec![
                "Null Notes/Work/.nullnotes/ids.json", "Null Notes/Work/.nullnotes/lock.json", "Null Notes/Work/.nullnotes/settings.json",
                "Null Notes/Work/Projects/Plan.md", "Null Notes/Work/Projects/pic.png", "Null Notes/Work/Projects/todo.txt",
                "Null Notes/Work/Welcome.md", "Null Notes/Work/secret.md.lock",
            ], "no .nntmp, no .pdf, locked note as-is");
            assert_eq!(g.files["Null Notes/Work/secret.md.lock"].data, b"NNLOCK1\nciphertext");
            assert!(g.dirs.contains("Null Notes/Work/Empty"), "empty folders kept");
        }
        assert_eq!(r.uploaded.len(), 8);
        assert!(r.problems.is_empty(), "{:?}", r.problems);

        // second run: nothing changed, nothing re-hashed or re-sent
        let puts = m.s.lock().unwrap().puts;
        let r = run_once(&root, f, &d, &mut st).unwrap();
        assert_eq!((r.uploaded.len(), r.unchanged), (0, 8));
        assert_eq!(m.s.lock().unwrap().puts, puts);

        // touch without change: mtime differs, hash same -> no upload
        filetime::set_file_mtime(root.join("Projects/Plan.md"), filetime::FileTime::from_unix_time(1_900_000_000, 0)).unwrap();
        let r = run_once(&root, f, &d, &mut st).unwrap();
        assert_eq!(r.uploaded.len(), 0);

        // edit one note, delete one, delete a folder
        std::thread::sleep(Duration::from_millis(20));
        fs::write(root.join("Projects/Plan.md"), "# plan v2").unwrap();
        fs::remove_file(root.join("Projects/todo.txt")).unwrap();
        fs::remove_dir(root.join("Empty")).unwrap();
        let r = run_once(&root, f, &d, &mut st).unwrap();
        assert_eq!(r.uploaded, vec!["Projects/Plan.md".to_string()]);
        assert_eq!(r.deleted, vec!["Empty/".to_string(), "Projects/todo.txt".to_string()]);
        let g = m.s.lock().unwrap();
        assert_eq!(g.files["Null Notes/Work/Projects/Plan.md"].data, b"# plan v2");
        assert_eq!(g.files["Null Notes/Work/Projects/Plan.md"].versions, 2, "replaced, history kept by Drive");
        assert!(g.trash.contains(&"Null Notes/Work/Projects/todo.txt".to_string()), "deletes go to Drive's trash");
        assert!(!g.files.keys().any(|k| !k.starts_with("Null Notes/Work/")), "never writes outside the folder");
        drop(g);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn renames_become_moves_and_other_vaults_are_untouched() {
        let m = mock::start("tok");
        // another vault's backup in a sibling folder with a shared prefix
        m.s.lock().unwrap().files.insert("Null Notes/Work2/keep.md".into(), mock::F { data: b"x".to_vec(), sha: sha_hex(b"x"), mtime: None, versions: 1 });
        let root = vault();
        let d = drive(&m, "tok");
        let mut st = State::default();
        run_once(&root, "Null Notes/Work", &d, &mut st).unwrap();
        let puts = m.s.lock().unwrap().puts;
        crate::vault::Vault::open(root.clone()).unwrap().rename_path("Projects", "Archive/Projects").unwrap();
        st.pending_moves.push(("Projects".into(), "Archive/Projects".into()));
        let r = run_once(&root, "Null Notes/Work", &d, &mut st).unwrap();
        let g = m.s.lock().unwrap();
        assert_eq!(r.moved, 1);
        assert_eq!(g.moves, vec![("Null Notes/Work/Projects".to_string(), "Null Notes/Work/Archive/Projects".to_string())]);
        assert_eq!(g.puts, puts, "a move re-uploads nothing");
        assert!(g.files.contains_key("Null Notes/Work/Archive/Projects/Plan.md"));
        assert!(g.files.contains_key("Null Notes/Work2/keep.md"), "sibling folder untouched");
        assert!(g.trash.is_empty(), "{:?}", g.trash);
        drop(g);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn empty_or_missing_vault_never_empties_the_backup() {
        let m = mock::start("tok");
        let root = vault();
        let d = drive(&m, "tok");
        let mut st = State::default();
        run_once(&root, "Null Notes/Work", &d, &mut st).unwrap();
        let n = m.s.lock().unwrap().files.len();
        // the folder vanishes (unplugged drive)
        let moved = root.with_extension("away");
        fs::rename(&root, &moved).unwrap();
        assert!(run_once(&root, "Null Notes/Work", &d, &mut st).is_err());
        // an empty folder in its place
        fs::create_dir_all(&root).unwrap();
        let r = run_once(&root, "Null Notes/Work", &d, &mut st).unwrap();
        assert!(r.deleted.is_empty() && !r.problems.is_empty());
        assert_eq!(m.s.lock().unwrap().files.len(), n);
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&moved);
    }

    #[test]
    fn errors_token_e2ee_scope_retry_and_bad_names() {
        let m = mock::start("tok");
        let root = vault();
        let mut st = State::default();
        assert_eq!(run_once(&root, "N", &drive(&m, "wrong"), &mut st).unwrap_err(), DErr::Unauthorized);
        m.s.lock().unwrap().e2ee = true;
        assert_eq!(run_once(&root, "N", &drive(&m, "tok"), &mut st).unwrap_err(), DErr::E2ee);
        assert!(m.s.lock().unwrap().files.is_empty(), "nothing sent to an encrypted account");
        m.s.lock().unwrap().e2ee = false;
        m.s.lock().unwrap().search_only = true;
        assert_eq!(run_once(&root, "N", &drive(&m, "tok"), &mut st).unwrap_err(), DErr::Scope);
        m.s.lock().unwrap().search_only = false;
        // two 503s, then fine: retried with backoff
        m.s.lock().unwrap().fail_next = 2;
        // Windows itself can't create "CON.md"; elsewhere Drive refuses it and it's reported
        let reserved = fs::write(root.join("CON.md"), "reserved name").is_ok();
        let r = run_once(&root, "N", &drive(&m, "tok"), &mut st).unwrap();
        assert_eq!(r.uploaded.len(), 8);
        if reserved {
            assert_eq!(r.problems.len(), 1, "{:?}", r.problems);
            assert!(r.problems[0].contains("CON.md"));
        }
        // keeps failing: the run stops with a network error
        m.s.lock().unwrap().fail_next = 99;
        assert!(matches!(run_once(&root, "N", &drive(&m, "tok"), &mut st).unwrap_err(), DErr::Transient(_)));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scheduler_debounces_and_publishes() {
        let m = mock::start("tok");
        let root = vault();
        let cfgdir = root.with_extension("cfg");
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let s2 = seen.clone();
        let b = Backup::new(&cfgdir, &cfgdir, Box::new(move |s: &Status| s2.lock().unwrap().push(s.state.clone())));
        b.set_vault(Some((root.clone(), "Work".into())));
        assert_eq!(b.status().state, "off", "off by default");
        assert_eq!(b.status().folder, "Null Notes/Work");
        assert!(b.configure(true, &m.url, "", None, "backup").is_err(), "needs a token");
        let st = b.configure(true, &m.url, "/Null Notes/Work/", Some("tok"), "backup").unwrap();
        assert!(st.enabled && st.has_token);
        let wait = |pred: &dyn Fn() -> bool| { for _ in 0..300 { if pred() { return true; } std::thread::sleep(Duration::from_millis(20)); } false };
        assert!(wait(&|| b.status().last_ok.is_some()), "first run after turning it on");
        assert_eq!(m.s.lock().unwrap().files.len(), 8);
        assert!(!fs::read_to_string(cfgdir.join("backup.json")).unwrap().contains("tok"), "token not in the config file");
        // turning it off stops runs
        b.configure(false, &m.url, "", None, "backup").unwrap();
        fs::write(root.join("new.md"), "x").unwrap();
        b.kick();
        std::thread::sleep(Duration::from_millis(200));
        assert!(!m.s.lock().unwrap().files.contains_key("Null Notes/Work/new.md"));
        assert!(seen.lock().unwrap().contains(&"running".to_string()));
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&cfgdir);
    }

    #[test]
    fn server_and_folder_rules() {
        assert_eq!(clean_server("HTTPS://Drive.Example.com/").unwrap(), "https://drive.example.com");
        assert_eq!(clean_server("https://example.com/drive/").unwrap(), "https://example.com/drive");
        assert!(clean_server("http://192.168.1.20:8080").is_ok());
        assert!(clean_server("http://nas.lan").is_ok());
        assert!(clean_server("http://localhost:9000").is_ok());
        assert!(clean_server("http://drive.example.com").is_err());
        assert!(clean_server("ftp://x").is_err());
        assert!(clean_server("https://user@x").is_err());
        assert_eq!(clean_folder("/Null Notes/Work/").unwrap(), "Null Notes/Work");
        assert!(clean_folder("Null Notes/../x").is_err());
        assert!(clean_folder(" / ").is_err());
        assert_eq!(enc_path("Null Notes/Work/a b+c.md"), "Null%20Notes/Work/a%20b%2Bc.md");
        assert_eq!(parse_ms(&rfc3339_ms(1_760_000_000_123)), Some(1_760_000_000_123));
    }
}
