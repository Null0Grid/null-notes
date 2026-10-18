//! Stable note ids (search contract v1: "Every Null Note has a stable id").
//!
//! - `.md` notes carry `id: <uuid>` in YAML frontmatter on disk. The editor
//!   never sees that line: it is stripped on read and put back on every save,
//!   so it can't be deleted or mangled by accident and moves with the file.
//! - `.txt`, `.html` and `.canvas` can't hold frontmatter without changing
//!   what they are, so their ids live in `<vault>/.nullnotes/ids.json`
//!   (path -> id), updated by the app's own rename/move/delete.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::Mutex;

/// ids.json is touched by commands and by the indexer thread.
pub static IDS_LOCK: Mutex<()> = Mutex::new(());

pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub fn is_md(rel: &str) -> bool {
    let l = rel.to_ascii_lowercase();
    l.ends_with(".md") || l.ends_with(".markdown")
}

/// Byte range of the frontmatter body and of the whole block (incl. fences).
fn fm_bounds(text: &str) -> Option<(usize, usize, usize)> {
    let first = if text.starts_with("---\n") {
        4
    } else if text.starts_with("---\r\n") {
        5
    } else {
        return None;
    };
    let mut pos = first;
    for line in text[first..].split_inclusive('\n') {
        let t = line.trim_end_matches(['\r', '\n']);
        if t == "---" || t == "..." {
            return Some((first, pos, pos + line.len()));
        }
        pos += line.len();
    }
    None
}

fn id_line(line: &str) -> Option<String> {
    let t = line.trim_end_matches(['\r', '\n']);
    let v = t.strip_prefix("id:")?.trim().trim_matches(['"', '\'']).trim();
    (!v.is_empty()).then(|| v.to_string())
}

pub fn get_id(text: &str) -> Option<String> {
    let (s, e, _) = fm_bounds(text)?;
    text[s..e].split_inclusive('\n').find_map(id_line)
}

/// (frontmatter text, body after it). No frontmatter -> ("", whole text).
pub fn split_frontmatter(text: &str) -> (&str, &str) {
    match fm_bounds(text) {
        Some((s, e, end)) => (&text[s..e], &text[end..]),
        None => ("", text),
    }
}

/// Remove the id line; drop the frontmatter block if nothing else is in it.
pub fn strip_id(text: &str) -> String {
    let Some((s, e, end)) = fm_bounds(text) else { return text.to_string() };
    let kept: String = text[s..e].split_inclusive('\n').filter(|l| id_line(l).is_none()).collect();
    if kept.trim().is_empty() {
        return text[end..].to_string();
    }
    format!("{}{}{}", &text[..s], kept, &text[e..])
}

/// Put `id` into the frontmatter (creating it if needed). Any existing id is replaced.
pub fn with_id(text: &str, id: &str) -> String {
    let body = strip_id(text);
    match fm_bounds(&body) {
        Some((s, _, _)) => {
            let nl = if body[..s].ends_with("\r\n") { "\r\n" } else { "\n" };
            format!("{}id: {id}{nl}{}", &body[..s], &body[s..])
        }
        None => format!("---\nid: {id}\n---\n{body}"),
    }
}

/* ---------------- ids.json ---------------- */

fn ids_path(root: &Path) -> std::path::PathBuf {
    root.join(crate::vault::META_DIR).join("ids.json")
}

pub fn load_ids(root: &Path) -> BTreeMap<String, String> {
    fs::read(ids_path(root)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn save_ids(root: &Path, ids: &BTreeMap<String, String>) {
    let p = ids_path(root);
    let _ = fs::create_dir_all(p.parent().unwrap());
    let _ = crate::vault::atomic_write(&p, serde_json::to_string_pretty(ids).unwrap().as_bytes());
}

/// Id for a non-markdown note, created on first use.
pub fn sidecar_id(root: &Path, rel: &str) -> String {
    let _g = IDS_LOCK.lock().unwrap();
    let mut ids = load_ids(root);
    if let Some(id) = ids.get(rel) {
        return id.clone();
    }
    let id = new_id();
    ids.insert(rel.to_string(), id.clone());
    save_ids(root, &ids);
    id
}

pub fn sidecar_set(root: &Path, rel: &str, id: &str) {
    let _g = IDS_LOCK.lock().unwrap();
    let mut ids = load_ids(root);
    ids.insert(rel.to_string(), id.to_string());
    save_ids(root, &ids);
}

/// Follow a rename/move of a file or a whole folder.
pub fn sidecar_move(root: &Path, from: &str, to: &str) {
    let _g = IDS_LOCK.lock().unwrap();
    let mut ids = load_ids(root);
    let pre = format!("{from}/");
    let moved: Vec<(String, String)> = ids
        .iter()
        .filter(|(p, _)| p.as_str() == from || p.starts_with(&pre))
        .map(|(p, id)| (p.clone(), id.clone()))
        .collect();
    if moved.is_empty() {
        return;
    }
    for (p, id) in moved {
        ids.remove(&p);
        ids.insert(format!("{to}{}", &p[from.len()..]), id);
    }
    save_ids(root, &ids);
}

pub fn sidecar_remove(root: &Path, rel: &str) {
    let _g = IDS_LOCK.lock().unwrap();
    let mut ids = load_ids(root);
    let pre = format!("{rel}/");
    let before = ids.len();
    ids.retain(|p, _| p != rel && !p.starts_with(&pre));
    if ids.len() != before {
        save_ids(root, &ids);
    }
}

/// Make sure the .md file at `abs` has an id in its frontmatter, keeping its
/// modified time so a backfill doesn't make every note look freshly edited.
pub fn ensure_md_id(abs: &Path) -> Option<String> {
    let bytes = fs::read(abs).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    if let Some(id) = get_id(&text) {
        return Some(id);
    }
    let id = new_id();
    set_md_id(abs, &text, &id)?;
    Some(id)
}

/// Rewrite the .md at `abs` with `id`, preserving its mtime.
pub fn set_md_id(abs: &Path, text: &str, id: &str) -> Option<()> {
    let mtime = fs::metadata(abs).ok().map(|m| filetime::FileTime::from_last_modification_time(&m));
    crate::vault::atomic_write(abs, with_id(text, id).as_bytes()).ok()?;
    if let Some(t) = mtime {
        let _ = filetime::set_file_mtime(abs, t);
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_roundtrip() {
        let plain = "# Note\n\nbody\n";
        let a = with_id(plain, "abc");
        assert_eq!(a, "---\nid: abc\n---\n# Note\n\nbody\n");
        assert_eq!(get_id(&a).as_deref(), Some("abc"));
        assert_eq!(strip_id(&a), plain);

        let fm = "---\ntags: [x]\n---\nbody";
        let b = with_id(fm, "x1");
        assert_eq!(b, "---\nid: x1\ntags: [x]\n---\nbody");
        assert_eq!(strip_id(&b), fm);
        assert_eq!(get_id(&with_id(&b, "x2")).as_deref(), Some("x2"));

        let crlf = "---\r\nid: \"q\"\r\ntitle: t\r\n---\r\nbody";
        assert_eq!(get_id(crlf).as_deref(), Some("q"));
        assert_eq!(strip_id(crlf), "---\r\ntitle: t\r\n---\r\nbody");
        // a horizontal rule mid-note is not frontmatter
        assert_eq!(get_id("text\n---\nid: no\n---\n"), None);
    }
}
