//! Two-way sync executor (Phase 7): scans both sides, asks `syncplan` what to
//! do, does it carefully, and records the new base.
//!
//! The Drive side is behind [`Remote`] so the engine can be tested with two
//! "devices" against one in-memory Drive. The HTTP implementation lives in
//! `drive_remote` and follows Drive's published API.
//!
//! Rules on top of the plan:
//! - Before a local file is replaced or deleted, it is hashed again. If it
//!   changed since the scan (the user typed, git pulled), Drive's version is
//!   saved as a conflict copy instead, and nothing local is lost.
//! - Every Drive write carries the version the plan saw (`Expect`). If Drive
//!   moved on meanwhile, that step is skipped and the run asks to be retried.
//! - Local deletes go to the Recycle Bin, Drive deletes to Drive's trash.
//! - `.nullnotes/ids.json` isn't synced as a file: both sides' maps are merged.

use crate::backup::{sha256_file, DErr};
use crate::noteid;
use crate::syncplan::{self, Act, BItem, Expect, LItem, RItem};
use crate::vault::{atomic_write, Vault, META_DIR};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::Path;
use std::time::UNIX_EPOCH;

pub const IDS_JSON: &str = ".nullnotes/ids.json";

/* ======================= the Drive side ======================= */

#[derive(Debug, Clone, PartialEq)]
pub struct RFile {
    /// relative to the vault's Drive folder
    pub path: String,
    pub drive_id: String,
    pub version: i64,
    pub sha: String,
    /// Drive's `synced_id` (read from Markdown frontmatter on upload)
    pub synced_id: Option<String>,
}

/// Paths are relative to the vault's folder on Drive.
pub trait Remote {
    fn list(&self) -> Result<Vec<RFile>, DErr>;
    fn get(&self, path: &str) -> Result<Vec<u8>, DErr>;
    /// `Expect::Absent`: create only. `Version(v)`: replace only if still at v.
    fn put(&self, path: &str, data: &[u8], mtime_ms: i64, expect: &Expect) -> Result<(), DErr>;
    fn delete(&self, path: &str, version: i64) -> Result<(), DErr>;
    fn mv(&self, from: &str, to: &str, version: i64) -> Result<(), DErr>;
    /// ids.json is merged, not versioned: last writer wins on the file itself.
    fn put_any(&self, path: &str, data: &[u8]) -> Result<(), DErr>;
}

/* ======================= Null Drive over HTTP ======================= */

/// [`Remote`] for a vault folder on Null Drive (server 0.8.2+), per
/// `null-grid/notes-sync-drive-api.md`: versions are ETags, writes are
/// conditional (`If-Match: "<v>"` / `If-None-Match: *`), a mismatch is
/// `409 conflict` → `DErr::Precondition` → the engine re-plans.
pub struct DriveRemote<'a> {
    pub drive: &'a crate::backup::Drive,
    /// "Null Notes/Work", no slashes at the ends
    pub folder: String,
}

impl DriveRemote<'_> {
    fn full(&self, rel: &str) -> String {
        format!("{}/{}", self.folder, rel)
    }
    fn files_url(&self, rel: &str) -> String {
        format!("/api/v1/files/{}", crate::backup::enc_path(&self.full(rel)))
    }
}

impl Remote for DriveRemote<'_> {
    fn list(&self) -> Result<Vec<RFile>, DErr> {
        let url = format!("/api/v1/files?prefix={}", crate::backup::enc_path(&self.folder));
        let v: serde_json::Value = self
            .drive
            .retry("listing", || self.drive.req("GET", &url).call())?
            .into_json()
            .map_err(|e| DErr::Other(format!("listing: {e}")))?;
        let pre = format!("{}/", self.folder);
        Ok(v.get("files")
            .and_then(|f| f.as_array())
            .map(|a| a.as_slice())
            .unwrap_or(&[])
            .iter()
            .filter(|f| !f.get("is_dir").and_then(|x| x.as_bool()).unwrap_or(false))
            .filter(|f| !f.get("deleted").and_then(|x| x.as_bool()).unwrap_or(false))
            .filter_map(|f| {
                let path = f.get("path")?.as_str()?.trim_matches('/');
                Some(RFile {
                    path: path.strip_prefix(&pre)?.to_string(),
                    drive_id: f.get("id")?.as_str()?.to_string(),
                    version: f.get("version")?.as_i64()?,
                    sha: f.get("sha256").and_then(|x| x.as_str()).unwrap_or("").to_ascii_lowercase(),
                    synced_id: f.get("synced_id").and_then(|x| x.as_str()).map(|s| s.to_lowercase()),
                })
            })
            .collect())
    }

    fn get(&self, rel: &str) -> Result<Vec<u8>, DErr> {
        let url = self.files_url(rel);
        self.drive.retry_with(|| {
            let r = self.drive.req("GET", &url).call().map_err(|e| crate::backup::Drive::classify(e, rel))?;
            let want = r.header("X-Content-SHA256").map(|s| s.to_ascii_lowercase());
            let mut buf = Vec::new();
            r.into_reader().read_to_end(&mut buf).map_err(|e| DErr::Transient(format!("{rel}: {e}")))?;
            if let Some(w) = want {
                if w != crate::backup::sha256_hex(&buf) {
                    return Err(DErr::Transient(format!("{rel}: download was cut short")));
                }
            }
            Ok(buf)
        })
    }

    fn put(&self, rel: &str, data: &[u8], mtime_ms: i64, expect: &Expect) -> Result<(), DErr> {
        let url = self.files_url(rel);
        let sha = crate::backup::sha256_hex(data);
        self.drive.retry_with(|| {
            let mut req = self.drive.req("PUT", &url)
                .set("X-Content-SHA256", &sha) // makes a retried write idempotent (200, not 409)
                .set("X-NullDrive-Mtime", &crate::backup::rfc3339_ms(mtime_ms))
                .set("Content-Type", "application/octet-stream");
            req = match expect {
                Expect::Absent => req.set("If-None-Match", "*"),
                Expect::Version(v) => req.set("If-Match", &format!("\"{v}\"")),
            };
            req.send_bytes(data).map(|_| ()).map_err(|e| crate::backup::Drive::classify(e, rel))
        })
    }

    fn put_any(&self, rel: &str, data: &[u8]) -> Result<(), DErr> {
        let url = self.files_url(rel);
        let sha = crate::backup::sha256_hex(data);
        self.drive.retry(rel, || {
            self.drive.req("PUT", &url).set("If-Match", "*").set("X-Content-SHA256", &sha).set("Content-Type", "application/json").send_bytes(data)
        }).map(|_| ())
    }

    fn delete(&self, rel: &str, version: i64) -> Result<(), DErr> {
        let url = self.files_url(rel);
        match self.drive.retry(rel, || self.drive.req("DELETE", &url).set("If-Match", &format!("\"{version}\"")).call()) {
            Ok(_) => Ok(()),
            // 404: already gone is fine; 409 conflict: edited on Drive since -> re-plan
            Err(DErr::Precondition(m)) => {
                if self.list()?.iter().any(|f| f.path == rel) { Err(DErr::Precondition(m)) } else { Ok(()) }
            }
            Err(e) => Err(e),
        }
    }

    fn mv(&self, from: &str, to: &str, version: i64) -> Result<(), DErr> {
        let body = serde_json::json!({ "from": self.full(from), "to": self.full(to), "if_match": version });
        self.drive
            .retry(from, || self.drive.req("POST", "/api/v1/files/move").set("If-Match", &format!("\"{version}\"")).send_json(body.clone()))
            .map(|_| ())
    }
}

/* ======================= state ======================= */

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Cached {
    pub size: u64,
    pub mtime_ms: i64,
    pub sha: String,
    pub note_id: Option<String>,
}

/// Per vault, in the app data folder (never in the vault).
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct SyncState {
    #[serde(default)]
    pub server: String,
    #[serde(default)]
    pub folder: String,
    #[serde(default)]
    pub base: Vec<BItem>,
    #[serde(default)]
    pub cache: BTreeMap<String, Cached>,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize)]
pub struct SyncReport {
    pub uploaded: Vec<String>,
    pub downloaded: Vec<String>,
    pub moved: Vec<String>,
    pub deleted_local: Vec<String>,
    pub deleted_remote: Vec<String>,
    /// conflict copies made (local paths)
    pub conflicts: Vec<String>,
    pub problems: Vec<String>,
    /// something changed under us (Drive or disk): run again soon
    pub retry: bool,
}

/* ======================= local scan ======================= */

fn mtime_ms(md: &fs::Metadata) -> i64 {
    md.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn note_id_of(rel: &str, abs: &Path, ids: &BTreeMap<String, String>) -> Option<String> {
    if noteid::is_md(rel) {
        let mut head = vec![0u8; 4096];
        let n = fs::File::open(abs).and_then(|mut f| std::io::Read::read(&mut f, &mut head)).unwrap_or(0);
        head.truncate(n);
        noteid::get_id(&String::from_utf8_lossy(&head)).map(|s| s.to_lowercase())
    } else {
        ids.get(rel).map(|s| s.to_lowercase())
    }
}

/// Everything the vault lists + `.nullnotes/*.json` except ids.json.
fn scan_local(root: &Path, cache: &mut BTreeMap<String, Cached>) -> Result<Vec<LItem>, String> {
    let set = crate::backup::local_set(root)?;
    let ids = noteid::load_ids(root);
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for (rel, (abs, size, mtime)) in &set.files {
        if rel == IDS_JSON {
            continue;
        }
        let c = match cache.get(rel) {
            Some(c) if c.size == *size && c.mtime_ms == *mtime => c.clone(),
            _ => {
                let Ok(sha) = sha256_file(abs) else { continue };
                let c = Cached { size: *size, mtime_ms: *mtime, sha, note_id: note_id_of(rel, abs, &ids) };
                cache.insert(rel.clone(), c.clone());
                c
            }
        };
        // non-md ids can change without the file changing (ids.json)
        let note_id = if noteid::is_md(rel) { c.note_id.clone() } else { ids.get(rel).map(|s| s.to_lowercase()) };
        seen.insert(rel.clone());
        out.push(LItem { path: rel.clone(), sha: c.sha, note_id });
    }
    cache.retain(|k, _| seen.contains(k));
    Ok(out)
}

fn to_items(files: &[RFile], ids: &BTreeMap<String, String>) -> Vec<RItem> {
    files
        .iter()
        .filter(|f| f.path != IDS_JSON && !f.path.ends_with(".nntmp"))
        .map(|f| RItem {
            path: f.path.clone(),
            drive_id: f.drive_id.clone(),
            version: f.version,
            sha: f.sha.clone(),
            note_id: if noteid::is_md(&f.path) { f.synced_id.clone().map(|s| s.to_lowercase()) } else { ids.get(&f.path).map(|s| s.to_lowercase()) },
        })
        .collect()
}

fn remote_ids(remote: &dyn Remote, files: &[RFile]) -> Result<BTreeMap<String, String>, DErr> {
    if !files.iter().any(|f| f.path == IDS_JSON) {
        return Ok(BTreeMap::new());
    }
    let b = remote.get(IDS_JSON)?;
    Ok(serde_json::from_slice(&b).unwrap_or_default())
}

/* ======================= local file ops ======================= */

fn sha_now(abs: &Path) -> Option<String> {
    sha256_file(abs).ok()
}

/// Write bytes from Drive to a new or existing local file.
fn write_local(v: &Vault, rel: &str, data: &[u8]) -> Result<(), String> {
    let abs = v.abs(rel);
    if let Some(d) = abs.parent() {
        fs::create_dir_all(d).map_err(|e| format!("{rel}: {e}"))?;
    }
    atomic_write(&abs, data)
}

/// A copy of Drive's version as a separate note: new note id.
fn conflict_bytes(rel_copy: &str, data: &[u8]) -> Vec<u8> {
    if noteid::is_md(rel_copy) {
        let t = String::from_utf8_lossy(data);
        noteid::with_id(&noteid::strip_id(&t), &noteid::new_id()).into_bytes()
    } else {
        data.to_vec()
    }
}

fn is_textnote(rel: &str) -> bool {
    matches!(crate::vault::kind_of(rel), Some("txt" | "html" | "canvas"))
}

fn make_conflict_copy(v: &Vault, rel_copy: &str, data: &[u8], remote: &dyn Remote, rep: &mut SyncReport) -> Result<(), DErr> {
    let bytes = conflict_bytes(rel_copy, data);
    write_local(v, rel_copy, &bytes).map_err(DErr::Other)?;
    if is_textnote(rel_copy) {
        let _ = noteid::sidecar_id(&v.root, rel_copy); // new id for the copy
    }
    rep.conflicts.push(rel_copy.to_string());
    let mtime = fs::metadata(v.abs(rel_copy)).map(|m| mtime_ms(&m)).unwrap_or(0);
    match remote.put(rel_copy, &bytes, mtime, &Expect::Absent) {
        Ok(()) => rep.uploaded.push(rel_copy.to_string()),
        Err(DErr::Precondition(_)) => rep.retry = true,
        Err(e @ (DErr::File(_) | DErr::Other(_))) => rep.problems.push(e.user_message()),
        Err(e) => return Err(e),
    }
    Ok(())
}

/// A txt/html/canvas note renamed outside the app loses its id (ids.json is
/// keyed by path). If its content is exactly that of a synced note whose old
/// path is gone, it is that note: give the id back, so the rename becomes a
/// Drive move (history kept) instead of delete + new file.
fn reattach_ids(v: &Vault, litems: &mut [LItem], base: &[BItem]) {
    let present: HashSet<String> = litems.iter().map(|l| l.path.to_lowercase()).collect();
    let mut used = HashSet::new();
    for l in litems.iter_mut().filter(|l| l.note_id.is_none() && is_textnote(&l.path)) {
        if let Some(b) = base.iter().find(|b| {
            b.key.starts_with("n:") && !present.contains(&b.local_path.to_lowercase()) && b.local_sha == l.sha && !used.contains(&b.key)
        }) {
            used.insert(b.key.clone());
            let id = b.key[2..].to_string();
            noteid::sidecar_set(&v.root, &l.path, &id);
            l.note_id = Some(id);
        }
    }
}

/* ======================= one sync run ======================= */

pub fn conflict_name_for(v: &Vault) -> impl Fn(&str, &HashSet<String>) -> String + '_ {
    move |p: &str, taken: &HashSet<String>| {
        let mut c = v.conflict_path(p);
        // also unique against names the plan already handed out
        let mut n = 2;
        while taken.contains(&c.to_lowercase()) {
            let base = v.conflict_path(p);
            let (stem, ext) = match base.rfind(')') {
                Some(i) => (&base[..i], &base[i..]),
                None => (base.as_str(), ""),
            };
            c = format!("{stem} {n}{ext}");
            n += 1;
        }
        c
    }
}

/// Scan, plan and execute one sync. `Err` = the run stopped (token, network…).
pub fn run_sync(root: &Path, remote: &dyn Remote, st: &mut SyncState, allow_mass_delete: bool) -> Result<SyncReport, DErr> {
    let v = Vault::open(root.to_path_buf()).map_err(|_| DErr::Other(format!("The vault folder {} isn't there (unplugged drive?). Nothing was synced.", root.display())))?;
    let mut rep = SyncReport::default();

    let rfiles = remote.list()?;
    let rids = remote_ids(remote, &rfiles)?;
    let ritems = to_items(&rfiles, &rids);
    let mut litems = scan_local(root, &mut st.cache).map_err(DErr::Other)?;
    reattach_ids(&v, &mut litems, &st.base);

    let cname = conflict_name_for(&v);
    let plan = syncplan::plan(&litems, &ritems, &st.base, &cname, allow_mass_delete);
    rep.problems.extend(plan.problems.clone());
    let lsha: HashMap<&str, &str> = litems.iter().map(|l| (l.path.as_str(), l.sha.as_str())).collect();
    let rmap: HashMap<&str, &RItem> = ritems.iter().map(|r| (r.path.as_str(), r)).collect();

    for act in &plan.acts {
        match act {
            Act::MoveLocal { from, to } => {
                let (fa, ta) = (v.abs(from), v.abs(to));
                if ta.exists() && from.to_lowercase() != to.to_lowercase() {
                    rep.retry = true;
                    continue;
                }
                if let Some(d) = ta.parent() {
                    let _ = fs::create_dir_all(d);
                }
                match fs::rename(&fa, &ta) {
                    Ok(()) => {
                        noteid::sidecar_move(&v.root, from, to);
                        if let Some(c) = st.cache.remove(from) {
                            st.cache.insert(to.clone(), c);
                        }
                        rep.moved.push(format!("{from} → {to}"));
                    }
                    Err(e) => rep.problems.push(format!("{from}: couldn't rename it ({e})")),
                }
            }
            Act::MoveRemote { from, to, version } => match remote.mv(from, to, *version) {
                Ok(()) => rep.moved.push(format!("{from} → {to} (Drive)")),
                Err(DErr::Precondition(_)) => rep.retry = true,
                Err(e @ (DErr::File(_) | DErr::Other(_))) => rep.problems.push(e.user_message()),
                Err(e) => return Err(e),
            },
            Act::DeleteLocal { path, sha } => {
                let abs = v.abs(path);
                if sha_now(&abs).as_deref() != Some(sha.as_str()) {
                    rep.retry = true; // edited since the scan: keep it
                    continue;
                }
                match trash::delete(&abs) {
                    Ok(()) => {
                        noteid::sidecar_remove(&v.root, path);
                        st.cache.remove(path);
                        rep.deleted_local.push(path.clone());
                    }
                    // never fall back to a permanent delete
                    Err(e) => rep.problems.push(format!("{path}: deleted on Drive, but couldn't move it to the Recycle Bin here ({e})")),
                }
            }
            Act::DeleteRemote { path, version } => match remote.delete(path, *version) {
                Ok(()) => rep.deleted_remote.push(path.clone()),
                Err(DErr::Precondition(_)) => rep.retry = true,
                Err(e @ (DErr::File(_) | DErr::Other(_))) => rep.problems.push(e.user_message()),
                Err(e) => return Err(e),
            },
            Act::ConflictCopy { remote: rp, copy, .. } => {
                let data = remote.get(rp)?;
                make_conflict_copy(&v, copy, &data, remote, &mut rep)?;
            }
            Act::Download { remote: rp, local, expect_local, .. } => {
                let data = remote.get(rp)?;
                let abs = v.abs(local);
                let now = if abs.exists() { sha_now(&abs) } else { None };
                if now != *expect_local {
                    // changed (or created) here since the scan: keep both
                    let copy = cname(local, &HashSet::new());
                    make_conflict_copy(&v, &copy, &data, remote, &mut rep)?;
                    continue;
                }
                write_local(&v, local, &data).map_err(DErr::Other)?;
                if is_textnote(local) {
                    if let Some(id) = rids.get(rp) {
                        noteid::sidecar_set(&v.root, local, id);
                    }
                }
                st.cache.remove(local);
                rep.downloaded.push(local.clone());
            }
            Act::Upload { local, remote: rp, expect } => {
                let abs = v.abs(local);
                let Ok(data) = fs::read(&abs) else { continue };
                // the plan was made from this content; if it changed, just retry
                if lsha.get(local.as_str()).map(|s| *s != crate::backup::sha256_hex(&data)).unwrap_or(false) && rmap.contains_key(local.as_str()) {
                    rep.retry = true;
                }
                let mtime = fs::metadata(&abs).map(|m| mtime_ms(&m)).unwrap_or(0);
                match remote.put(rp, &data, mtime, expect) {
                    Ok(()) => rep.uploaded.push(rp.clone()),
                    Err(DErr::Precondition(_)) => rep.retry = true,
                    Err(e @ (DErr::File(_) | DErr::Other(_))) => rep.problems.push(e.user_message()),
                    Err(e) => return Err(e),
                }
            }
        }
    }

    // name collisions renamed by the plan are conflicts the user should hear about too
    for c in &plan.conflicts {
        if !rep.conflicts.contains(c) && v.abs(c).exists() {
            rep.conflicts.push(c.clone());
        }
    }

    // ids.json: merge both maps (local wins), keep paths that exist somewhere
    let after_r = remote.list()?;
    let after_l = scan_local(root, &mut st.cache).map_err(DErr::Other)?;
    {
        let _g = noteid::IDS_LOCK.lock().unwrap();
        let local_ids = noteid::load_ids(root);
        let mut merged = rids.clone();
        merged.extend(local_ids.clone());
        let exists: HashSet<&str> = after_l.iter().map(|l| l.path.as_str()).chain(after_r.iter().map(|r| r.path.as_str())).collect();
        merged.retain(|p, _| exists.contains(p.as_str()));
        let local_view: BTreeMap<String, String> = merged.iter().filter(|(p, _)| after_l.iter().any(|l| &l.path == *p)).map(|(a, b)| (a.clone(), b.clone())).collect();
        if local_view != local_ids && !local_view.is_empty() {
            noteid::save_ids(root, &local_view);
        }
        if merged != rids && !merged.is_empty() {
            let bytes = serde_json::to_vec_pretty(&merged).unwrap();
            if let Err(e) = remote.put_any(IDS_JSON, &bytes) {
                match e {
                    DErr::File(_) | DErr::Other(_) | DErr::Precondition(_) => rep.retry = true,
                    e => return Err(e),
                }
            }
        }
    }

    // New base: what is now the same on both sides. Keys seen on one side
    // only keep their old base entry, so deletes are still recognised later.
    let rids2 = remote_ids(remote, &after_r)?;
    let ritems2 = to_items(&after_r, &rids2);
    let litems2 = scan_local(root, &mut st.cache).map_err(DErr::Other)?;
    let (lk, rk) = syncplan::keys(&litems2, &ritems2, &st.base);
    let mut base = Vec::new();
    for (k, l) in &lk {
        if let Some(r) = rk.get(k) {
            if l.sha == r.sha {
                base.push(BItem { key: k.clone(), local_path: l.path.clone(), local_sha: l.sha.clone(), drive_id: r.drive_id.clone(), remote_path: r.path.clone(), remote_version: r.version, remote_sha: r.sha.clone() });
                continue;
            }
        }
        if let Some(old) = st.base.iter().find(|b| &b.key == k) {
            base.push(old.clone());
        }
    }
    for (k, _) in rk.iter().filter(|(k, _)| !lk.contains_key(*k)) {
        if let Some(old) = st.base.iter().find(|b| &b.key == k) {
            base.push(old.clone());
        }
    }
    st.base = base;
    rep.problems.truncate(50);
    let _ = META_DIR;
    Ok(rep)
}

/* ======================= tests: two devices, one Drive ======================= */

#[cfg(test)]
pub mod mem {
    //! In-memory Drive with versions, stable ids, conditional writes.
    use super::*;
    use std::sync::Mutex;

    #[derive(Clone, Debug)]
    pub struct F {
        pub id: String,
        pub version: i64,
        pub data: Vec<u8>,
    }

    #[derive(Default)]
    pub struct Mem {
        pub files: Mutex<BTreeMap<String, F>>,
        pub trash: Mutex<Vec<String>>,
        pub next: Mutex<i64>,
        /// called before each write: lets a test change Drive "concurrently"
        pub before_write: Mutex<Option<Box<dyn FnMut(&mut BTreeMap<String, F>) + Send>>>,
    }

    impl Mem {
        fn bump(&self) -> i64 {
            let mut n = self.next.lock().unwrap();
            *n += 1;
            *n
        }
        fn hook(&self) {
            let mut h = self.before_write.lock().unwrap();
            if let Some(f) = h.as_mut() {
                f(&mut self.files.lock().unwrap());
            }
            *h = None;
        }
        pub fn text(&self, p: &str) -> Option<String> {
            self.files.lock().unwrap().get(p).map(|f| String::from_utf8_lossy(&f.data).into_owned())
        }
    }

    impl Remote for Mem {
        fn list(&self) -> Result<Vec<RFile>, DErr> {
            Ok(self
                .files
                .lock()
                .unwrap()
                .iter()
                .map(|(p, f)| RFile {
                    path: p.clone(),
                    drive_id: f.id.clone(),
                    version: f.version,
                    sha: crate::backup::sha256_hex(&f.data),
                    synced_id: if noteid::is_md(p) { noteid::get_id(&String::from_utf8_lossy(&f.data)) } else { None },
                })
                .collect())
        }
        fn get(&self, path: &str) -> Result<Vec<u8>, DErr> {
            self.files.lock().unwrap().get(path).map(|f| f.data.clone()).ok_or(DErr::Other(format!("{path}: not found")))
        }
        fn put(&self, path: &str, data: &[u8], _m: i64, expect: &Expect) -> Result<(), DErr> {
            self.hook();
            let v = self.bump();
            let mut g = self.files.lock().unwrap();
            match (g.get(path), expect) {
                (Some(_), Expect::Absent) => return Err(DErr::Precondition(path.into())),
                (Some(f), Expect::Version(x)) if f.version != *x => return Err(DErr::Precondition(path.into())),
                (None, Expect::Version(_)) => return Err(DErr::Precondition(path.into())),
                _ => {}
            }
            let id = g.get(path).map(|f| f.id.clone()).unwrap_or_else(|| format!("{v:032x}"));
            g.insert(path.into(), F { id, version: v, data: data.to_vec() });
            Ok(())
        }
        fn put_any(&self, path: &str, data: &[u8]) -> Result<(), DErr> {
            let v = self.bump();
            let mut g = self.files.lock().unwrap();
            let id = g.get(path).map(|f| f.id.clone()).unwrap_or_else(|| format!("{v:032x}"));
            g.insert(path.into(), F { id, version: v, data: data.to_vec() });
            Ok(())
        }
        fn delete(&self, path: &str, version: i64) -> Result<(), DErr> {
            self.hook();
            let mut g = self.files.lock().unwrap();
            match g.get(path) {
                Some(f) if f.version == version => {
                    g.remove(path);
                    self.trash.lock().unwrap().push(path.into());
                    Ok(())
                }
                Some(_) => Err(DErr::Precondition(path.into())),
                None => Ok(()),
            }
        }
        fn mv(&self, from: &str, to: &str, version: i64) -> Result<(), DErr> {
            self.hook();
            let v = self.bump();
            let mut g = self.files.lock().unwrap();
            match g.get(from) {
                Some(f) if f.version == version && !g.contains_key(to) => {
                    let mut f = g.remove(from).unwrap();
                    f.version = v; // same id, new version
                    g.insert(to.into(), f);
                    Ok(())
                }
                _ => Err(DErr::Precondition(from.into())),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::mem::Mem;
    use super::*;

    struct Dev {
        root: std::path::PathBuf,
        st: SyncState,
    }
    impl Dev {
        fn new(name: &str) -> Dev {
            let root = std::env::temp_dir().join(format!("nn-sync-{}-{}-{}", std::process::id(), name, crate::backup::now_rfc3339().replace(':', "")));
            fs::create_dir_all(&root).unwrap();
            Dev { root, st: SyncState::default() }
        }
        fn v(&self) -> Vault {
            Vault::open(self.root.clone()).unwrap()
        }
        fn sync(&mut self, d: &Mem) -> SyncReport {
            let r = run_sync(&self.root, d, &mut self.st, false).unwrap();
            if r.retry {
                // what the scheduler does
                let r2 = run_sync(&self.root, d, &mut self.st, false).unwrap();
                return SyncReport { conflicts: [r.conflicts, r2.conflicts.clone()].concat(), ..r2 };
            }
            r
        }
        fn read(&self, p: &str) -> String {
            self.v().read_text(p).unwrap()
        }
        fn files(&self) -> Vec<String> {
            let mut v: Vec<String> = self.v().list_entries().unwrap().into_iter().filter(|e| !e.dir).map(|e| e.path).collect();
            v.sort();
            v
        }
        fn touch_later(&self) {
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
    }
    impl Drop for Dev {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn two_devices_converge() {
        let d = Mem::default();
        let (mut a, mut b) = (Dev::new("a"), Dev::new("b"));
        a.v().write_text("Plan.md", "# plan").unwrap();
        a.v().write_text("Projects/todo.txt", "[ ] x").unwrap();
        fs::write(a.root.join("pic.png"), b"\x89PNG").unwrap();
        a.sync(&d);
        let r = b.sync(&d);
        assert_eq!(b.files(), a.files());
        assert_eq!(b.read("Plan.md"), "# plan");
        assert_eq!(r.downloaded.len(), 3, "{r:?}");
        // the txt keeps its note id on the other device
        assert_eq!(noteid::load_ids(&a.root).get("Projects/todo.txt"), noteid::load_ids(&b.root).get("Projects/todo.txt"));
        // nothing to do afterwards, on either side
        let ra = a.sync(&d);
        let rb = b.sync(&d);
        assert!(ra.uploaded.is_empty() && ra.downloaded.is_empty() && rb.uploaded.is_empty() && rb.downloaded.is_empty(), "{ra:?} {rb:?}");
        // an edit travels
        b.touch_later();
        b.v().write_text("Plan.md", "# plan v2").unwrap();
        b.sync(&d);
        a.sync(&d);
        assert_eq!(a.read("Plan.md"), "# plan v2");
    }

    #[test]
    fn offline_edits_on_both_keep_both() {
        let d = Mem::default();
        let (mut a, mut b) = (Dev::new("a"), Dev::new("b"));
        a.v().write_text("Plan.md", "base").unwrap();
        a.sync(&d);
        b.sync(&d);
        a.touch_later();
        a.v().write_text("Plan.md", "from A").unwrap();
        b.v().write_text("Plan.md", "from B").unwrap();
        a.sync(&d); // A first: no conflict for A yet
        let rb = b.sync(&d);
        assert_eq!(rb.conflicts.len(), 1, "{rb:?}");
        let copy = &rb.conflicts[0];
        assert!(copy.starts_with("Plan (conflict ") && copy.ends_with(").md"));
        assert_eq!(b.read("Plan.md"), "from B");
        assert_eq!(b.read(copy), "from A");
        a.sync(&d);
        // both devices end with both versions, nothing lost
        assert_eq!(a.files(), b.files());
        let texts: Vec<String> = a.files().iter().filter(|p| p.ends_with(".md")).map(|p| a.read(p)).collect();
        assert!(texts.contains(&"from A".to_string()) && texts.contains(&"from B".to_string()), "{texts:?}");
        // the copy is a separate note (its own id)
        let id = |dev: &Dev, p: &str| noteid::get_id(&fs::read_to_string(dev.root.join(p)).unwrap()).unwrap();
        assert_ne!(id(&a, "Plan.md"), id(&a, copy));
        assert_eq!(id(&a, copy), id(&b, copy));
    }

    #[test]
    fn renames_follow_the_note_id() {
        let d = Mem::default();
        let (mut a, mut b) = (Dev::new("a"), Dev::new("b"));
        a.v().write_text("Ideas.md", "v1").unwrap();
        a.sync(&d);
        b.sync(&d);
        let drive_id = d.files.lock().unwrap()["Ideas.md"].id.clone();
        // A renames in Explorer (no app involved); B edits, both offline
        fs::create_dir_all(a.root.join("Archive")).unwrap();
        fs::rename(a.root.join("Ideas.md"), a.root.join("Archive/Old ideas.md")).unwrap();
        b.touch_later();
        b.v().write_text("Ideas.md", "v2 from B").unwrap();
        a.sync(&d);
        assert_eq!(d.files.lock().unwrap()["Archive/Old ideas.md"].id, drive_id, "a Drive move, history kept");
        b.sync(&d);
        a.sync(&d);
        assert_eq!(a.files(), vec!["Archive/Old ideas.md".to_string()]);
        assert_eq!(b.files(), vec!["Archive/Old ideas.md".to_string()]);
        assert_eq!(a.read("Archive/Old ideas.md"), "v2 from B");
        assert!(d.trash.lock().unwrap().is_empty());
    }

    #[test]
    fn delete_vs_edit_and_plain_deletes() {
        let d = Mem::default();
        let (mut a, mut b) = (Dev::new("a"), Dev::new("b"));
        a.v().write_text("keep.md", "k").unwrap();
        a.v().write_text("gone.md", "g").unwrap();
        a.v().write_text("x.md", "x").unwrap();
        a.sync(&d);
        b.sync(&d);
        // A deletes gone.md and x.md; B edits x.md
        fs::remove_file(a.root.join("gone.md")).unwrap();
        fs::remove_file(a.root.join("x.md")).unwrap();
        b.touch_later();
        b.v().write_text("x.md", "x edited").unwrap();
        b.sync(&d); // B uploads its edit first
        let ra = a.sync(&d);
        assert_eq!(ra.deleted_remote, vec!["gone.md".to_string()]);
        assert_eq!(ra.downloaded, vec!["x.md".to_string()], "the edit wins over the delete");
        let rb = b.sync(&d);
        assert_eq!(rb.deleted_local, vec!["gone.md".to_string()], "to the Recycle Bin");
        assert_eq!(a.files(), b.files());
        assert_eq!(d.trash.lock().unwrap().as_slice(), ["gone.md"]);
    }

    #[test]
    fn drive_changes_during_a_run_are_never_overwritten() {
        let d = Mem::default();
        let (mut a, mut b) = (Dev::new("a"), Dev::new("b"));
        a.v().write_text("n.md", "v1").unwrap();
        a.sync(&d);
        b.sync(&d);
        a.touch_later();
        a.v().write_text("n.md", "A's edit").unwrap();
        // between A's listing and its upload, another device saves to Drive
        let id = d.files.lock().unwrap()["n.md"].id.clone();
        *d.before_write.lock().unwrap() = Some(Box::new(move |g| {
            let old = g["n.md"].clone();
            g.insert("n.md".into(), mem::F { id: id.clone(), version: old.version + 100, data: noteid::with_id("C's edit", &noteid::get_id(&String::from_utf8_lossy(&old.data)).unwrap()).into_bytes() });
        }));
        let r = run_sync(&a.root, &d, &mut a.st, false).unwrap();
        assert!(r.retry, "the upload saw a newer Drive version and backed off");
        assert!(d.text("n.md").unwrap().contains("C's edit"));
        let r = a.sync(&d);
        assert_eq!(r.conflicts.len(), 1);
        let texts: Vec<String> = a.files().iter().map(|p| a.read(p)).collect();
        assert!(texts.contains(&"A's edit".into()) && texts.contains(&"C's edit".into()), "{texts:?}");
    }

    #[test]
    fn local_edit_during_download_is_kept() {
        let d = Mem::default();
        let (mut a, mut b) = (Dev::new("a"), Dev::new("b"));
        a.v().write_text("n.md", "v1").unwrap();
        a.sync(&d);
        b.sync(&d);
        a.touch_later();
        a.v().write_text("n.md", "v2 from A").unwrap();
        a.sync(&d);
        // B plans a download, but the user types before it is written
        let rfiles = d.list().unwrap();
        let ritems = to_items(&rfiles, &BTreeMap::new());
        let litems = scan_local(&b.root, &mut b.st.cache).unwrap();
        let v = b.v();
        let cname = conflict_name_for(&v);
        let plan = syncplan::plan(&litems, &ritems, &b.st.base, &cname, false);
        assert!(matches!(plan.acts[0], Act::Download { .. }));
        b.touch_later();
        b.v().write_text("n.md", "B typing").unwrap();
        // run the same plan now, as the executor would
        let mut rep = SyncReport::default();
        if let Act::Download { remote: rp, local, expect_local, .. } = &plan.acts[0] {
            let now = sha_now(&v.abs(local));
            assert_ne!(&now, expect_local);
            make_conflict_copy(&v, &cname(local, &HashSet::new()), &d.get(rp).unwrap(), &d, &mut rep).unwrap();
        }
        assert_eq!(b.read("n.md"), "B typing", "never overwritten");
        assert_eq!(b.read(&rep.conflicts[0]), "v2 from A");
    }

    #[test]
    fn same_name_created_offline_on_both() {
        let d = Mem::default();
        let (mut a, mut b) = (Dev::new("a"), Dev::new("b"));
        a.v().write_text("Ideas.md", "A's ideas").unwrap();
        b.v().write_text("Ideas.md", "B's ideas").unwrap();
        a.sync(&d);
        let rb = b.sync(&d);
        assert_eq!(rb.conflicts.len(), 1, "{rb:?}");
        a.sync(&d);
        assert_eq!(a.files(), b.files());
        assert_eq!(a.files().len(), 2);
        assert_eq!(a.read("Ideas.md"), "A's ideas", "the one on Drive first keeps the name");
    }
}

#[cfg(test)]
pub mod httpmock {
    //! Null Drive's Phase 7 semantics over real HTTP (notes-sync-drive-api.md).
    use crate::noteid;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    use tiny_http::{Header, Response, Server};

    #[derive(Clone, Debug)]
    pub struct F {
        pub id: String,
        pub version: i64,
        pub data: Vec<u8>,
    }

    #[derive(Default)]
    pub struct S {
        pub files: BTreeMap<String, F>,
        pub trash: Vec<String>,
        pub counter: i64,
        pub log: Vec<String>,
        pub hook: Option<Box<dyn FnMut(&mut S) + Send>>,
    }

    impl S {
        pub fn bump(&mut self) -> i64 {
            self.counter += 1;
            self.counter
        }
    }

    pub struct Mock {
        pub url: String,
        pub s: Arc<Mutex<S>>,
    }

    fn sha(b: &[u8]) -> String {
        crate::backup::sha256_hex(b)
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
    fn row(p: &str, f: &F) -> serde_json::Value {
        let synced = if noteid::is_md(p) { noteid::get_id(&String::from_utf8_lossy(&f.data)) } else { None };
        serde_json::json!({"id": f.id, "path": p, "is_dir": false, "size": f.data.len(), "sha256": sha(&f.data), "version": f.version, "deleted": false, "synced_id": synced})
    }

    pub fn start(token: &str) -> Mock {
        let server = Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", server.server_addr().to_ip().unwrap().port());
        let s = Arc::new(Mutex::new(S::default()));
        let st = s.clone();
        let token = token.to_string();
        std::thread::spawn(move || {
            for mut req in server.incoming_requests() {
                let j = |code: u16, v: serde_json::Value| Response::from_string(v.to_string()).with_status_code(code).with_header(Header::from_bytes("Content-Type", "application/json").unwrap());
                let err = |code: u16, c: &str, cur: Option<serde_json::Value>| j(code, serde_json::json!({"error": {"code": c, "message": c, "current": cur}}));
                let hdr = |r: &tiny_http::Request, n: &str| r.headers().iter().find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(n)).map(|h| h.value.to_string());
                if hdr(&req, "Authorization") != Some(format!("Bearer {token}")) {
                    let _ = req.respond(err(401, "unauthorized", None));
                    continue;
                }
                let method = req.method().to_string();
                let url = req.url().to_string();
                let (path, query) = url.split_once('?').map(|(a, b)| (a.to_string(), b.to_string())).unwrap_or((url.clone(), String::new()));
                let mut body = Vec::new();
                let _ = req.as_reader().read_to_end(&mut body);
                let if_match = hdr(&req, "If-Match").map(|v| v.trim_matches('"').to_string());
                let if_none = hdr(&req, "If-None-Match");
                let claimed = hdr(&req, "X-Content-SHA256");
                let mut g = st.lock().unwrap();
                g.log.push(format!("{method} {}", dec(&path)));
                if method != "GET" {
                    if let Some(mut h) = g.hook.take() {
                        h(&mut g);
                    }
                }
                match (method.as_str(), path.as_str()) {
                    ("GET", "/api/v1/me") => { let _ = req.respond(j(200, serde_json::json!({"user":{"name":"luke","e2ee":"off"}}))); }
                    ("POST", "/api/v1/folders") => { let _ = req.respond(j(200, serde_json::json!({}))); }
                    ("GET", "/api/v1/files") => {
                        let prefix = dec(query.split('&').find_map(|kv| kv.strip_prefix("prefix=")).unwrap_or(""));
                        let rows: Vec<serde_json::Value> = g.files.iter().filter(|(p, _)| p.starts_with(&format!("{prefix}/"))).map(|(p, f)| row(p, f)).collect();
                        let v = g.counter;
                        let _ = req.respond(j(200, serde_json::json!({"version": v, "files": rows})));
                    }
                    ("GET", p) if p.starts_with("/api/v1/files/") => {
                        let rp = dec(&p["/api/v1/files/".len()..]);
                        match g.files.get(&rp) {
                            Some(f) => {
                                let r = Response::from_data(f.data.clone())
                                    .with_header(Header::from_bytes("ETag", format!("\"{}\"", f.version)).unwrap())
                                    .with_header(Header::from_bytes("X-Content-SHA256", sha(&f.data)).unwrap())
                                    .with_header(Header::from_bytes("X-NullDrive-File-Id", f.id.clone()).unwrap());
                                let _ = req.respond(r);
                            }
                            None => { let _ = req.respond(err(404, "not_found", None)); }
                        }
                    }
                    ("PUT", p) if p.starts_with("/api/v1/files/") => {
                        let rp = dec(&p["/api/v1/files/".len()..]);
                        let cur = g.files.get(&rp).cloned();
                        let same = cur.as_ref().map(|f| claimed.as_deref() == Some(sha(&f.data).as_str())).unwrap_or(false);
                        let curj = cur.as_ref().map(|f| row(&rp, f));
                        let ok = match (&if_none, &if_match, &cur) {
                            (Some(_), Some(_), _) => { let _ = req.respond(err(400, "invalid_if_match", None)); continue; }
                            (Some(n), None, c) => { if n != "*" { let _ = req.respond(err(400, "invalid_if_match", None)); continue; } c.is_none() || same }
                            (None, Some(m), _) if m == "*" => true,
                            (None, Some(m), Some(f)) => f.version.to_string() == *m || same,
                            (None, Some(_), None) => false,
                            (None, None, Some(_)) => { let _ = req.respond(err(428, "precondition_required", None)); continue; }
                            (None, None, None) => true,
                        };
                        if !ok { let _ = req.respond(err(409, "conflict", curj)); continue; }
                        if same { let _ = req.respond(j(200, curj.unwrap())); continue; }
                        let v = g.bump();
                        let id = cur.map(|f| f.id).unwrap_or_else(|| format!("{v:032x}"));
                        let f = F { id, version: v, data: body };
                        let r = row(&rp, &f);
                        g.files.insert(rp, f);
                        let _ = req.respond(j(200, r));
                    }
                    ("DELETE", p) if p.starts_with("/api/v1/files/") => {
                        let rp = dec(&p["/api/v1/files/".len()..]);
                        match g.files.get(&rp).cloned() {
                            None => { let _ = req.respond(err(404, "not_found", None)); }
                            Some(f) if if_match.as_deref() != Some("*") && if_match != Some(f.version.to_string()) => { let _ = req.respond(err(409, "conflict", Some(row(&rp, &f)))); }
                            Some(_) => { g.files.remove(&rp); g.trash.push(rp); let v = g.bump(); let _ = req.respond(j(200, serde_json::json!({"version": v}))); }
                        }
                    }
                    ("POST", "/api/v1/files/move") => {
                        let b: serde_json::Value = serde_json::from_slice(&body).unwrap();
                        let (from, to) = (b["from"].as_str().unwrap().to_string(), b["to"].as_str().unwrap().to_string());
                        let want = b["if_match"].as_i64().map(|v| v.to_string()).or(if_match.clone());
                        match g.files.get(&from).cloned() {
                            None => { let _ = req.respond(err(404, "not_found", None)); }
                            Some(f) if want != Some(f.version.to_string()) => { let _ = req.respond(err(409, "conflict", Some(row(&from, &f)))); }
                            Some(_) if g.files.contains_key(&to) => { let _ = req.respond(err(409, "target_exists", None)); }
                            Some(mut f) => { g.files.remove(&from); f.version = g.bump(); let v = f.version; g.files.insert(to, f); let _ = req.respond(j(200, serde_json::json!({"version": v}))); }
                        }
                    }
                    _ => { let _ = req.respond(err(404, "not_found", None)); }
                }
            }
        });
        Mock { url, s }
    }
}

#[cfg(test)]
mod http_tests {
    use super::*;

    const FOLDER: &str = "Null Notes/Work";

    struct Dev {
        root: std::path::PathBuf,
        st: SyncState,
        drive: crate::backup::Drive,
    }
    impl Dev {
        fn new(name: &str, url: &str) -> Dev {
            let root = std::env::temp_dir().join(format!("nn-hsync-{}-{}-{}", std::process::id(), name, crate::backup::now_rfc3339().replace(':', "")));
            fs::create_dir_all(&root).unwrap();
            let mut drive = crate::backup::Drive::new(url, "tok").unwrap();
            drive.backoff = vec![];
            Dev { root, st: SyncState::default(), drive }
        }
        fn sync(&mut self) -> SyncReport {
            let r = DriveRemote { drive: &self.drive, folder: FOLDER.into() };
            let mut out = run_sync(&self.root, &r, &mut self.st, false).unwrap();
            for _ in 0..3 {
                if !out.retry { break; }
                let more = run_sync(&self.root, &r, &mut self.st, false).unwrap();
                out = SyncReport { conflicts: [out.conflicts, more.conflicts.clone()].concat(), ..more };
            }
            out
        }
        fn v(&self) -> Vault { Vault::open(self.root.clone()).unwrap() }
        fn read(&self, p: &str) -> String { self.v().read_text(p).unwrap() }
        fn files(&self) -> Vec<String> {
            let mut v: Vec<String> = self.v().list_entries().unwrap().into_iter().filter(|e| !e.dir).map(|e| e.path).collect();
            v.sort();
            v
        }
    }
    impl Drop for Dev {
        fn drop(&mut self) { let _ = fs::remove_dir_all(&self.root); }
    }
    fn later() { std::thread::sleep(std::time::Duration::from_millis(15)); }

    #[test]
    fn over_http_two_devices_conflicts_renames_deletes() {
        let m = httpmock::start("tok");
        let (mut a, mut b) = (Dev::new("a", &m.url), Dev::new("b", &m.url));
        a.v().write_text("Plan.md", "base").unwrap();
        a.v().write_text("Inbox/todo.txt", "[ ] a").unwrap();
        a.v().write_text("Trash me.md", "bye").unwrap();
        fs::write(a.root.join("pic.png"), b"\x89PNG").unwrap();
        a.sync();
        b.sync();
        assert_eq!(a.files(), b.files());
        assert_eq!(noteid::load_ids(&a.root).get("Inbox/todo.txt"), noteid::load_ids(&b.root).get("Inbox/todo.txt"));
        assert!(m.s.lock().unwrap().files.contains_key("Null Notes/Work/.nullnotes/ids.json"));

        // offline on both: conflicting edits, a rename, a delete, an image rename
        later();
        a.v().write_text("Plan.md", "from A").unwrap();
        b.v().write_text("Plan.md", "from B").unwrap();
        fs::create_dir_all(b.root.join("Done")).unwrap();
        fs::rename(b.root.join("Inbox/todo.txt"), b.root.join("Done/todo.txt")).unwrap(); // Explorer rename: id via content? txt id is in ids.json by path -> see below
        crate::vault::Vault::open(b.root.clone()).unwrap(); // (no-op; Explorer has no hook)
        fs::remove_file(a.root.join("Trash me.md")).unwrap();
        fs::create_dir_all(a.root.join("img")).unwrap();
        fs::rename(a.root.join("pic.png"), a.root.join("img/pic.png")).unwrap();

        let ra = a.sync();
        let rb = b.sync();
        let ra2 = a.sync();
        assert!(ra.conflicts.is_empty() && ra2.problems.is_empty(), "{ra:?} {ra2:?}");
        assert_eq!(rb.conflicts.len(), 1, "{rb:?}");
        assert_eq!(a.files(), b.files(), "both devices end the same");
        let texts: Vec<String> = a.files().iter().filter(|p| p.ends_with(".md")).map(|p| a.read(p)).collect();
        assert!(texts.contains(&"from A".into()) && texts.contains(&"from B".into()), "{texts:?}");
        assert!(!a.files().contains(&"Trash me.md".to_string()) && !b.files().contains(&"Trash me.md".to_string()));
        assert!(b.files().contains(&"img/pic.png".to_string()), "image move followed by content");
        assert!(a.files().contains(&"Done/todo.txt".to_string()));
        assert_eq!(noteid::load_ids(&a.root).get("Done/todo.txt"), noteid::load_ids(&b.root).get("Done/todo.txt"), "the txt kept its note id");
        assert!(noteid::load_ids(&a.root).get("Done/todo.txt").is_some());
        let g = m.s.lock().unwrap();
        assert_eq!(g.trash, vec!["Null Notes/Work/Trash me.md".to_string()], "the renamed txt was moved, not deleted");
        assert!(g.log.iter().any(|l| l.starts_with("POST /api/v1/files/move")), "moves used, not re-uploads");
        assert!(!g.log.iter().any(|l| l.contains("428")));
    }

    #[test]
    fn over_http_concurrent_write_is_never_overwritten() {
        let m = httpmock::start("tok");
        let (mut a, mut b) = (Dev::new("a", &m.url), Dev::new("b", &m.url));
        a.v().write_text("n.md", "v1").unwrap();
        a.sync();
        b.sync();
        later();
        a.v().write_text("n.md", "A's edit").unwrap();
        // just before A's first write, another device saves to Drive
        m.s.lock().unwrap().hook = Some(Box::new(|g: &mut httpmock::S| {
            let k = "Null Notes/Work/n.md".to_string();
            let mut f = g.files[&k].clone();
            let id = noteid::get_id(&String::from_utf8_lossy(&f.data)).unwrap();
            f.data = noteid::with_id("C's edit", &id).into_bytes();
            f.version = g.bump();
            g.files.insert(k, f);
        }));
        let r = a.sync();
        assert_eq!(r.conflicts.len(), 1, "{r:?}");
        let texts: Vec<String> = a.files().iter().map(|p| a.read(p)).collect();
        assert!(texts.contains(&"A's edit".into()) && texts.contains(&"C's edit".into()), "{texts:?}");
        b.sync();
        assert_eq!(a.files(), b.files());
    }
}
