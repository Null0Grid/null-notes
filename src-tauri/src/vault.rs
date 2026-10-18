//! Everything the app does to the vault folder. No Tauri types here, so it can
//! be unit-tested and reused by the search agent later.

use crate::lock::{self, Key, LockFile};
use crate::noteid;
use base64::{engine::general_purpose, Engine};
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

pub const META_DIR: &str = ".nullnotes";
const TMP_EXT: &str = "nntmp";
const TEXT_EDIT_MAX: u64 = 5 * 1024 * 1024; // read_text / write_text
const BULK_TEXT_MAX: u64 = 1024 * 1024; // read_all_text skips bigger files; the UI reads them on demand
const FILE_MAX: u64 = 50 * 1024 * 1024; // images and imports
const META_MAX: usize = 2 * 1024 * 1024;

pub const TEXT_KINDS: [&str; 4] = ["md", "txt", "html", "canvas"];

pub type R<T> = Result<T, String>;

/// Today's date for conflict names, in local time where the OS tells us.
pub fn conflict_date() -> String {
    let now = time::OffsetDateTime::now_local().unwrap_or_else(|_| time::OffsetDateTime::now_utc());
    format!("{:04}-{:02}-{:02}", now.year(), now.month() as u8, now.day())
}

#[derive(Serialize, Clone)]
pub struct Entry {
    pub path: String,
    pub name: String,
    pub dir: bool,
    pub kind: String,
    pub size: u64,
    pub modified: u64,
}

#[derive(Serialize, Default)]
pub struct ImportReport {
    pub added: Vec<String>,
    pub skipped: Vec<String>,
}

fn ext_of(p: &str) -> String {
    let name = p.rsplit('/').next().unwrap_or(p);
    match name.rfind('.') {
        Some(i) if i > 0 => name[i + 1..].to_ascii_lowercase(),
        _ => String::new(),
    }
}

/// md | txt | html | canvas | img | locked, or None for files the vault ignores.
pub fn kind_of(p: &str) -> Option<&'static str> {
    let e = ext_of(p);
    if e == "lock" {
        let inner = &p[..p.len() - 5];
        return match kind_of(inner) {
            Some(k) if TEXT_KINDS.contains(&k) => Some("locked"),
            _ => None,
        };
    }
    Some(match e.as_str() {
        "md" | "markdown" => "md",
        "txt" => "txt",
        "html" | "htm" => "html",
        "canvas" => "canvas",
        "png" | "jpg" | "jpeg" => "img",
        _ => return None,
    })
}

pub fn is_text(p: &str) -> bool {
    matches!(kind_of(p), Some(k) if TEXT_KINDS.contains(&k))
}


fn file_name(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

/// Normalise a vault-relative path and refuse anything that could escape it.
pub fn clean(rel: &str) -> R<String> {
    let rel = rel.replace('\\', "/");
    let rel = rel.trim_matches('/');
    if rel.is_empty() {
        return Ok(String::new());
    }
    for part in rel.split('/') {
        if part.trim().is_empty() || part == "." || part == ".." {
            return Err(format!("\"{rel}\" isn't a valid path inside the vault."));
        }
        if part.chars().any(|c| "<>:\"|?*".contains(c) || (c as u32) < 32) {
            return Err(format!(
                "\"{part}\" contains a character Windows doesn't allow in names (< > : \" | ? *)."
            ));
        }
        if part.eq_ignore_ascii_case(META_DIR) {
            return Err(format!("\"{rel}\" isn't a valid path inside the vault."));
        }
    }
    Ok(rel.to_string())
}

fn mtime_ms(md: &fs::Metadata) -> u64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Write via a temp file and rename, so a crash never leaves half a note.
pub fn atomic_write(path: &Path, data: &[u8]) -> R<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".");
    tmp.push(TMP_EXT);
    let tmp = PathBuf::from(tmp);
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    fs::write(&tmp, data).map_err(|e| format!("Couldn't save {name}: {e}"))?;
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("Couldn't save {name}: {e}")
    })
}

pub struct Vault {
    pub root: PathBuf,
    pub key: Option<Key>,
}

impl Vault {
    pub fn open(root: PathBuf) -> R<Vault> {
        if !root.is_dir() {
            return Err(format!("{} isn't a folder.", root.display()));
        }
        Ok(Vault { root, key: None })
    }

    pub fn name(&self) -> String {
        self.root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| self.root.display().to_string())
    }

    pub fn abs(&self, rel: &str) -> PathBuf {
        let mut p = self.root.clone();
        for part in rel.split('/').filter(|s| !s.is_empty()) {
            p.push(part);
        }
        p
    }

    fn exists(&self, rel: &str) -> bool {
        self.abs(rel).symlink_metadata().is_ok()
    }

    fn unique(&self, dir: &str, name: &str) -> String {
        let join = |n: &str| if dir.is_empty() { n.to_string() } else { format!("{dir}/{n}") };
        if !self.exists(&join(name)) {
            return join(name);
        }
        let (stem, ext) = match name.rfind('.') {
            Some(i) if i > 0 => (&name[..i], &name[i..]),
            _ => (name, ""),
        };
        (1..).map(|n| join(&format!("{stem} {n}{ext}"))).find(|p| !self.exists(p)).unwrap()
    }

    /* ---------- listing and reading ---------- */

    pub fn list_entries(&self) -> R<Vec<Entry>> {
        let mut out = Vec::new();
        self.walk(&self.root, "", &mut out)?;
        out.sort_by(|a, b| a.path.to_lowercase().cmp(&b.path.to_lowercase()));
        Ok(out)
    }

    fn walk(&self, dir: &Path, rel: &str, out: &mut Vec<Entry>) -> R<()> {
        let rd = fs::read_dir(dir).map_err(|e| format!("Couldn't open {}: {e}", dir.display()))?;
        for ent in rd.flatten() {
            let name = ent.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || name.ends_with(&format!(".{TMP_EXT}")) {
                continue;
            }
            let Ok(md) = ent.metadata() else { continue };
            let path = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
            if md.is_dir() {
                out.push(Entry { path: path.clone(), name, dir: true, kind: "dir".into(), size: 0, modified: mtime_ms(&md) });
                self.walk(&ent.path(), &path, out)?;
            } else if let Some(k) = kind_of(&path) {
                out.push(Entry { path, name, dir: false, kind: k.into(), size: md.len(), modified: mtime_ms(&md) });
            }
        }
        Ok(())
    }

    pub fn read_all_text(&self) -> R<BTreeMap<String, String>> {
        let mut o = BTreeMap::new();
        for e in self.list_entries()? {
            if !e.dir && TEXT_KINDS.contains(&e.kind.as_str()) && e.size <= BULK_TEXT_MAX {
                if let Ok(s) = fs::read(self.abs(&e.path)) {
                    let t = String::from_utf8_lossy(&s);
                    let t = if noteid::is_md(&e.path) { noteid::strip_id(&t) } else { t.into_owned() };
                    o.insert(e.path, t);
                }
            }
        }
        Ok(o)
    }

    pub fn read_text(&self, rel: &str) -> R<String> {
        let p = clean(rel)?;
        if kind_of(&p) == Some("locked") {
            return Err("That note is locked. Unlock the vault to open it.".into());
        }
        if !is_text(&p) {
            return Err("Only .md, .txt, .html and .canvas files can be edited as text.".into());
        }
        let abs = self.abs(&p);
        let md = fs::metadata(&abs).map_err(|_| "That file doesn't exist anymore.".to_string())?;
        if md.len() > TEXT_EDIT_MAX {
            return Err(format!("{} is larger than 5 MB, which is too big to edit here.", file_name(&p)));
        }
        let b = fs::read(&abs).map_err(|e| format!("Couldn't read {}: {e}", file_name(&p)))?;
        let t = String::from_utf8_lossy(&b);
        Ok(if noteid::is_md(&p) { noteid::strip_id(&t) } else { t.into_owned() })
    }

    pub fn read_image(&self, rel: &str) -> R<String> {
        let p = clean(rel)?;
        if kind_of(&p) != Some("img") {
            return Err("Only .png and .jpg images are supported.".into());
        }
        let abs = self.abs(&p);
        let md = fs::metadata(&abs).map_err(|_| "That image doesn't exist anymore.".to_string())?;
        if md.len() > FILE_MAX {
            return Err(format!("{} is larger than 50 MB.", file_name(&p)));
        }
        let b = fs::read(&abs).map_err(|e| format!("Couldn't read {}: {e}", file_name(&p)))?;
        let mime = if b.starts_with(b"\x89PNG") { "image/png" } else { "image/jpeg" };
        Ok(format!("data:{mime};base64,{}", general_purpose::STANDARD.encode(b)))
    }

    /* ---------- writing ---------- */

    /// "Plan.md" -> "Plan (conflict 2026-10-18).md" (then "… 2", "… 3"), in the
    /// same folder. Locked notes keep both extensions: "Plan (conflict …).md.lock".
    pub fn conflict_path(&self, rel: &str) -> String {
        let (dir, name) = match rel.rfind('/') {
            Some(i) => (&rel[..i], &rel[i + 1..]),
            None => ("", rel),
        };
        let lower = name.to_ascii_lowercase();
        let ext_len = if lower.ends_with(".lock") {
            let inner = &name[..name.len() - 5];
            inner.rfind('.').map(|i| name.len() - i).unwrap_or(5)
        } else {
            name.rfind('.').filter(|&i| i > 0).map(|i| name.len() - i).unwrap_or(0)
        };
        let (stem, ext) = name.split_at(name.len() - ext_len);
        let date = conflict_date();
        let join = |n: String| if dir.is_empty() { n } else { format!("{dir}/{n}") };
        let first = join(format!("{stem} (conflict {date}){ext}"));
        if !self.exists(&first) {
            return first;
        }
        (2..).map(|n| join(format!("{stem} (conflict {date} {n}){ext}"))).find(|p| !self.exists(p)).unwrap()
    }

    /// Save from the editor. `base` is the text the edit started from. If the
    /// file on disk changed since (sync, git, another editor), the disk version
    /// stays and the edit is saved next to it as a conflict copy, whose path is
    /// returned. Nothing is ever silently overwritten.
    pub fn write_text_guarded(&self, rel: &str, content: &str, base: Option<&str>) -> R<Option<String>> {
        let p = clean(rel)?;
        if let Some(base) = base {
            if let Ok(b) = fs::read(self.abs(&p)) {
                let disk = String::from_utf8_lossy(&b);
                let disk = if noteid::is_md(&p) { noteid::strip_id(&disk) } else { disk.into_owned() };
                if disk != base && disk != content {
                    let copy = self.conflict_path(&p);
                    // a copy is a different note: new id (md frontmatter / ids.json)
                    let body = if noteid::is_md(&p) { noteid::with_id(&noteid::strip_id(content), &noteid::new_id()) } else { content.to_string() };
                    self.write_text(&copy, &body)?;
                    return Ok(Some(copy));
                }
            }
        }
        self.write_text(&p, content).map(|_| None)
    }

    pub fn write_text(&self, rel: &str, content: &str) -> R<()> {
        let p = clean(rel)?;
        if !is_text(&p) {
            return Err("Only .md, .txt, .html and .canvas files can be edited as text.".into());
        }
        if content.len() as u64 > TEXT_EDIT_MAX {
            return Err(format!("{} is larger than 5 MB, which is too big to edit here.", file_name(&p)));
        }
        let abs = self.abs(&p);
        if let Some(d) = abs.parent() {
            fs::create_dir_all(d).map_err(|e| format!("Couldn't save {}: {e}", file_name(&p)))?;
        }
        if noteid::is_md(&p) {
            // The editor never sees the id; keep the one on disk (or mint one).
            let id = noteid::get_id(content)
                .or_else(|| fs::read(&abs).ok().and_then(|b| noteid::get_id(&String::from_utf8_lossy(&b))))
                .unwrap_or_else(noteid::new_id);
            atomic_write(&abs, noteid::with_id(content, &id).as_bytes())
        } else {
            atomic_write(&abs, content.as_bytes())?;
            noteid::sidecar_id(&self.root, &p);
            Ok(())
        }
    }

    pub fn create_file(&self, rel: &str, content: &str) -> R<()> {
        let p = clean(rel)?;
        if !is_text(&p) {
            return Err("Only .md, .txt, .html and .canvas files can be created.".into());
        }
        if self.exists(&p) {
            return Err(format!("{p} already exists."));
        }
        self.write_text(&p, content)
    }

    pub fn write_image(&self, rel: &str, data_url: &str) -> R<()> {
        let p = clean(rel)?;
        if kind_of(&p) != Some("img") {
            return Err("Only .png and .jpg images are supported.".into());
        }
        if self.exists(&p) {
            return Err(format!("{p} already exists."));
        }
        let b64 = data_url.split_once(";base64,").map(|x| x.1).unwrap_or(data_url);
        let bytes = general_purpose::STANDARD.decode(b64.trim()).map_err(|_| "That image couldn't be read.".to_string())?;
        if bytes.len() as u64 > FILE_MAX {
            return Err(format!("{} is larger than 50 MB.", file_name(&p)));
        }
        let abs = self.abs(&p);
        if let Some(d) = abs.parent() {
            fs::create_dir_all(d).map_err(|e| format!("Couldn't save {}: {e}", file_name(&p)))?;
        }
        atomic_write(&abs, &bytes)
    }

    pub fn create_folder(&self, rel: &str) -> R<()> {
        let p = clean(rel)?;
        if p.is_empty() {
            return Err("Give the folder a name.".into());
        }
        if self.exists(&p) {
            return Err(format!("{p} already exists."));
        }
        fs::create_dir_all(self.abs(&p)).map_err(|e| format!("Couldn't create {p}: {e}"))
    }

    pub fn rename_path(&self, from: &str, to: &str) -> R<()> {
        let (from, to) = (clean(from)?, clean(to)?);
        if from.is_empty() || to.is_empty() {
            return Err("Give it a name.".into());
        }
        let (fa, ta) = (self.abs(&from), self.abs(&to));
        let is_dir = fa.is_dir();
        if !fa.exists() {
            return Err(format!("{from} doesn't exist anymore."));
        }
        let case_only = from.to_lowercase() == to.to_lowercase();
        if self.exists(&to) && !case_only {
            return Err(format!("{to} already exists."));
        }
        if is_dir && (to == from || to.starts_with(&format!("{from}/"))) {
            return Err("A folder can't be moved inside itself.".into());
        }
        if !is_dir && kind_of(&to).is_none() {
            return Err("Keep a .md, .txt, .html, .canvas, .png or .jpg extension so the file stays in the vault.".into());
        }
        if let Some(d) = ta.parent() {
            fs::create_dir_all(d).map_err(|e| format!("Couldn't move {from}: {e}"))?;
        }
        if case_only && from != to {
            // Windows treats Foo and foo as the same name, so go through a temp name.
            let mut tmp = fa.as_os_str().to_owned();
            tmp.push(".nnrename");
            fs::rename(&fa, &tmp).and_then(|_| fs::rename(&tmp, &ta)).map_err(|e| format!("Couldn't rename {from}: {e}"))?;
        } else {
            fs::rename(&fa, &ta).map_err(|e| format!("Couldn't move {from}: {e}"))?;
        }
        // .md ids travel inside the file; the others follow in ids.json.
        noteid::sidecar_move(&self.root, &from, &to);
        Ok(())
    }

    pub fn delete_path(&self, rel: &str, permanent: bool) -> R<()> {
        let p = clean(rel)?;
        if p.is_empty() {
            return Err("The vault itself can't be deleted from here.".into());
        }
        let abs = self.abs(&p);
        if !abs.exists() {
            return Ok(());
        }
        if !permanent {
            trash::delete(&abs).map_err(|e| format!("Couldn't move {p} to the Recycle Bin: {e}"))?;
        } else {
            if abs.is_dir() { fs::remove_dir_all(&abs) } else { fs::remove_file(&abs) }.map_err(|e| format!("Couldn't delete {p}: {e}"))?;
        }
        noteid::sidecar_remove(&self.root, &p);
        Ok(())
    }

    /* ---------- import ---------- */

    pub fn import_paths(&self, paths: &[String], dest: &str) -> R<ImportReport> {
        let dest = clean(dest)?;
        if !dest.is_empty() {
            fs::create_dir_all(self.abs(&dest)).map_err(|e| format!("Couldn't create {dest}: {e}"))?;
        }
        let mut rep = ImportReport::default();
        let root = fs::canonicalize(&self.root).unwrap_or(self.root.clone());
        for src in paths {
            let src = PathBuf::from(src);
            let name = src.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            if src.is_dir() {
                let canon = fs::canonicalize(&src).unwrap_or(src.clone());
                if root.starts_with(&canon) || canon.starts_with(&root) {
                    rep.skipped.push(format!("{name} (can't import a folder into itself)"));
                    continue;
                }
                let top = self.unique(&dest, &name);
                self.import_dir(&src, &top, &mut rep);
            } else {
                self.import_file(&src, &dest, &name, &mut rep);
            }
        }
        Ok(rep)
    }

    fn import_file(&self, src: &Path, dir: &str, name: &str, rep: &mut ImportReport) {
        if kind_of(name).is_none() || kind_of(name) == Some("locked") {
            rep.skipped.push(name.to_string());
            return;
        }
        if fs::metadata(src).map(|m| m.len() > FILE_MAX).unwrap_or(true) {
            rep.skipped.push(format!("{name} (over 50 MB)"));
            return;
        }
        let target = self.unique(dir, name);
        let abs = self.abs(&target);
        if let Some(d) = abs.parent() {
            let _ = fs::create_dir_all(d);
        }
        match fs::copy(src, &abs) {
            Ok(_) => {
                if noteid::is_md(&target) {
                    noteid::ensure_md_id(&abs);
                } else if is_text(&target) {
                    noteid::sidecar_id(&self.root, &target);
                }
                rep.added.push(target)
            }
            Err(_) => rep.skipped.push(name.to_string()),
        }
    }

    fn import_dir(&self, src: &Path, target: &str, rep: &mut ImportReport) {
        let _ = fs::create_dir_all(self.abs(target));
        let Ok(rd) = fs::read_dir(src) else { return };
        for ent in rd.flatten() {
            let name = ent.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let p = ent.path();
            if p.is_dir() {
                self.import_dir(&p, &format!("{target}/{name}"), rep);
            } else {
                self.import_file(&p, target, &name, rep);
            }
        }
    }

    /* ---------- per-vault meta (.nullnotes/<name>.json) ---------- */

    fn meta_path(&self, name: &str) -> R<PathBuf> {
        let ok = !name.is_empty() && name.len() <= 40 && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if !ok {
            return Err("Invalid settings name.".into());
        }
        Ok(self.root.join(META_DIR).join(format!("{name}.json")))
    }

    pub fn read_meta(&self, name: &str) -> R<Option<String>> {
        let p = self.meta_path(name)?;
        match fs::read(&p) {
            Ok(b) if b.len() > META_MAX => Err("Vault settings are too large.".into()),
            Ok(b) => Ok(Some(String::from_utf8_lossy(&b).into_owned())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("Couldn't read vault settings: {e}")),
        }
    }

    pub fn write_meta(&self, name: &str, content: &str) -> R<()> {
        let p = self.meta_path(name)?;
        if content.len() > META_MAX {
            return Err("Vault settings are too large.".into());
        }
        fs::create_dir_all(p.parent().unwrap()).map_err(|e| format!("Couldn't save vault settings: {e}"))?;
        atomic_write(&p, content.as_bytes())
    }

    /* ---------- lock ---------- */

    fn lock_file_path(&self) -> PathBuf {
        self.root.join(META_DIR).join("lock.json")
    }

    fn lock_file(&self) -> R<Option<LockFile>> {
        match fs::read(self.lock_file_path()) {
            Ok(b) => serde_json::from_slice(&b).map(Some).map_err(|_| "The vault's lock file is damaged.".into()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("Couldn't read lock.json: {e}")),
        }
    }

    fn save_lock_file(&self, lf: &LockFile) -> R<()> {
        let p = self.lock_file_path();
        fs::create_dir_all(p.parent().unwrap()).map_err(|e| format!("Couldn't save lock.json: {e}"))?;
        atomic_write(&p, serde_json::to_string_pretty(lf).unwrap().as_bytes())
    }

    pub fn lock_configured(&self) -> bool {
        self.lock_file_path().is_file()
    }

    fn key(&self) -> R<&Key> {
        self.key.as_ref().ok_or_else(|| "The vault is locked. Unlock it first.".to_string())
    }

    pub fn lock_setup(&mut self, pass: &str) -> R<()> {
        if self.lock_configured() {
            return Err("This vault already has a passphrase.".into());
        }
        if pass.chars().count() < 8 {
            return Err("Use a passphrase of at least 8 characters.".into());
        }
        let (lf, key) = lock::setup(pass)?;
        self.save_lock_file(&lf)?;
        self.key = Some(key);
        Ok(())
    }

    pub fn lock_unlock(&mut self, pass: &str) -> R<()> {
        let lf = self.lock_file()?.ok_or("This vault doesn't have a passphrase yet.")?;
        self.key = Some(lock::unlock(&lf, pass)?);
        Ok(())
    }

    pub fn lock_change(&mut self, old: &str, new: &str) -> R<()> {
        let lf = self.lock_file()?.ok_or("This vault doesn't have a passphrase yet.")?;
        if new.chars().count() < 8 {
            return Err("Use a passphrase of at least 8 characters.".into());
        }
        let key = lock::unlock(&lf, old)?;
        self.save_lock_file(&lock::wrap(new, &key)?)?;
        self.key = Some(key);
        Ok(())
    }

    pub fn encrypt_path(&self, rel: &str) -> R<String> {
        let key = self.key()?;
        let p = clean(rel)?;
        if !is_text(&p) {
            return Err("Only .md, .txt, .html and .canvas files can be locked.".into());
        }
        let np = format!("{p}.lock");
        if self.exists(&np) {
            return Err(format!("{np} already exists."));
        }
        let plain = fs::read(self.abs(&p)).map_err(|e| format!("Couldn't read {}: {e}", file_name(&p)))?;
        atomic_write(&self.abs(&np), &lock::encrypt_file(key, &plain)?)?;
        fs::remove_file(self.abs(&p)).map_err(|e| format!("Locked copy saved, but the original couldn't be removed: {e}"))?;
        noteid::sidecar_move(&self.root, &p, &np);
        Ok(np)
    }

    pub fn decrypt_path(&self, rel: &str) -> R<String> {
        let key = self.key()?;
        let p = clean(rel)?;
        if kind_of(&p) != Some("locked") {
            return Err("That isn't a locked note.".into());
        }
        let np = p[..p.len() - 5].to_string();
        if self.exists(&np) {
            return Err(format!("{np} already exists."));
        }
        let data = fs::read(self.abs(&p)).map_err(|e| format!("Couldn't read {}: {e}", file_name(&p)))?;
        atomic_write(&self.abs(&np), &lock::decrypt_file(key, &data, file_name(&p))?)?;
        fs::remove_file(self.abs(&p)).map_err(|e| format!("Unlocked copy saved, but the locked file couldn't be removed: {e}"))?;
        noteid::sidecar_move(&self.root, &p, &np);
        Ok(np)
    }

    pub fn read_locked(&self, rel: &str) -> R<String> {
        let key = self.key()?;
        let p = clean(rel)?;
        if kind_of(&p) != Some("locked") {
            return Err("That isn't a locked note.".into());
        }
        let data = fs::read(self.abs(&p)).map_err(|_| "That file doesn't exist anymore.".to_string())?;
        let plain = lock::decrypt_file(key, &data, file_name(&p))?;
        let t = String::from_utf8_lossy(&plain);
        Ok(if noteid::is_md(&p[..p.len() - 5]) { noteid::strip_id(&t) } else { t.into_owned() })
    }

    pub fn write_locked(&self, rel: &str, content: &str) -> R<()> {
        let key = self.key()?;
        let p = clean(rel)?;
        if kind_of(&p) != Some("locked") {
            return Err("That isn't a locked note.".into());
        }
        let abs = self.abs(&p);
        if let Some(d) = abs.parent() {
            fs::create_dir_all(d).map_err(|e| format!("Couldn't save {}: {e}", file_name(&p)))?;
        }
        let content = if noteid::is_md(&p[..p.len() - 5]) {
            let id = noteid::get_id(content)
                .or_else(|| {
                    let d = fs::read(&abs).ok()?;
                    let pl = lock::decrypt_file(key, &d, file_name(&p)).ok()?;
                    noteid::get_id(&String::from_utf8_lossy(&pl))
                })
                .unwrap_or_else(noteid::new_id);
            noteid::with_id(content, &id)
        } else {
            content.to_string()
        };
        atomic_write(&abs, &lock::encrypt_file(key, content.as_bytes())?)
    }

    pub fn create_locked(&self, rel: &str, content: &str) -> R<()> {
        let p = clean(rel)?;
        if self.exists(&p) {
            return Err(format!("{p} already exists."));
        }
        self.write_locked(&p, content)
    }

    pub fn read_all_locked(&self) -> R<BTreeMap<String, String>> {
        let key = self.key()?;
        let mut o = BTreeMap::new();
        for e in self.list_entries()? {
            if e.kind == "locked" {
                if let Ok(d) = fs::read(self.abs(&e.path)) {
                    if let Ok(pl) = lock::decrypt_file(key, &d, &e.name) {
                        let t = String::from_utf8_lossy(&pl);
                        let t = if noteid::is_md(&e.path[..e.path.len() - 5]) { noteid::strip_id(&t) } else { t.into_owned() };
                        o.insert(e.path, t);
                    }
                }
            }
        }
        Ok(o)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_vault() -> (Vault, PathBuf) {
        let d = std::env::temp_dir().join(format!("nn-test-{}-{}", std::process::id(), rand_suffix()));
        fs::create_dir_all(&d).unwrap();
        (Vault::open(d.clone()).unwrap(), d)
    }
    fn rand_suffix() -> u64 {
        std::time::SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64
    }

    #[test]
    fn paths_are_confined() {
        assert!(clean("../x.md").is_err());
        assert!(clean("a/../../x.md").is_err());
        assert!(clean(".nullnotes/lock.json").is_err());
        assert!(clean("a/b:c.md").is_err());
        assert_eq!(clean("\\a\\b.md/").unwrap(), "a/b.md");
    }

    #[test]
    fn guarded_save_keeps_both() {
        let (v, d) = tmp_vault();
        v.write_text("Plan.md", "v1").unwrap();
        // editor opened "v1"; meanwhile sync wrote "remote"
        let id = noteid::get_id(&fs::read_to_string(d.join("Plan.md")).unwrap()).unwrap();
        fs::write(d.join("Plan.md"), noteid::with_id("remote", &id)).unwrap();
        let copy = v.write_text_guarded("Plan.md", "mine", Some("v1")).unwrap().unwrap();
        assert!(copy.starts_with("Plan (conflict ") && copy.ends_with(").md"), "{copy}");
        assert_eq!(v.read_text("Plan.md").unwrap(), "remote");
        assert_eq!(v.read_text(&copy).unwrap(), "mine");
        let copy_id = noteid::get_id(&fs::read_to_string(d.join(&copy)).unwrap()).unwrap();
        assert_ne!(copy_id, id, "the copy is a separate note");
        // normal save: base matches disk
        assert_eq!(v.write_text_guarded("Plan.md", "remote + mine", Some("remote")).unwrap(), None);
        assert_eq!(v.read_text("Plan.md").unwrap(), "remote + mine");
        // same content already on disk is not a conflict
        assert_eq!(v.write_text_guarded("Plan.md", "remote + mine", Some("stale")).unwrap(), None);
        // names
        assert_eq!(v.conflict_path("a/Note.md.lock"), format!("a/Note (conflict {}).md.lock", conflict_date()));
        assert_eq!(v.conflict_path("todo.txt"), format!("todo (conflict {}).txt", conflict_date()));
        fs::write(d.join(format!("todo (conflict {}).txt", conflict_date())), "x").unwrap();
        assert_eq!(v.conflict_path("todo.txt"), format!("todo (conflict {} 2).txt", conflict_date()));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn kinds() {
        assert_eq!(kind_of("a/b.MD"), Some("md"));
        assert_eq!(kind_of("x.canvas.lock"), Some("locked"));
        assert_eq!(kind_of("x.png.lock"), None);
        assert_eq!(kind_of("x.pdf"), None);
    }

    #[test]
    fn file_ops_meta_and_lock() {
        let (mut v, d) = tmp_vault();
        v.create_folder("Projects").unwrap();
        v.create_file("Projects/Plan.md", "# Plan").unwrap();
        assert!(v.create_file("Projects/Plan.md", "x").is_err());
        v.write_text("Projects/Plan.md", "# Plan v2").unwrap();
        assert_eq!(v.read_text("Projects/Plan.md").unwrap(), "# Plan v2");
        v.rename_path("Projects/Plan.md", "Projects/plan.md").unwrap();
        v.rename_path("Projects", "Archive/Projects").unwrap();
        assert!(v.rename_path("Archive", "Archive/Projects/x").is_err());
        let ents = v.list_entries().unwrap();
        assert!(ents.iter().any(|e| e.path == "Archive/Projects/plan.md" && e.kind == "md"));

        v.write_meta("settings", "{\"a\":1}").unwrap();
        assert_eq!(v.read_meta("settings").unwrap().unwrap(), "{\"a\":1}");
        assert!(v.read_meta("../x").is_err());
        assert!(v.list_entries().unwrap().iter().all(|e| !e.path.starts_with(".nullnotes")));

        v.lock_setup("passphrase1").unwrap();
        let np = v.encrypt_path("Archive/Projects/plan.md").unwrap();
        assert_eq!(np, "Archive/Projects/plan.md.lock");
        assert!(v.read_text(&np).is_err());
        v.key = None;
        assert!(v.read_locked(&np).is_err());
        assert_eq!(v.lock_unlock("wrongwrong").unwrap_err(), "Wrong passphrase.");
        v.lock_change("passphrase1", "passphrase2").unwrap();
        v.key = None;
        v.lock_unlock("passphrase2").unwrap();
        assert_eq!(v.read_locked(&np).unwrap(), "# Plan v2");
        assert_eq!(v.read_all_locked().unwrap().len(), 1);
        assert_eq!(v.decrypt_path(&np).unwrap(), "Archive/Projects/plan.md");

        let png = "data:image/png;base64,iVBORw0KGgo=";
        v.write_image("Attachments/a.png", png).unwrap();
        assert!(v.read_image("Attachments/a.png").unwrap().starts_with("data:image/png;base64,"));

        let src = d.with_extension("src");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join("sub/n.md"), "hi").unwrap();
        fs::write(src.join("skip.pdf"), "x").unwrap();
        let rep = v.import_paths(&[src.to_string_lossy().into()], "").unwrap();
        assert_eq!(rep.added.len(), 1);
        assert_eq!(rep.skipped, vec!["skip.pdf"]);
        v.delete_path("Archive", true).unwrap();
        assert!(!d.join("Archive").exists());
        let _ = fs::remove_dir_all(&d);
        let _ = fs::remove_dir_all(&src);
    }
}
