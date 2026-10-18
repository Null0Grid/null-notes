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
    let litems = scan_local(root, &mut st.cache).map_err(DErr::Other)?;

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
