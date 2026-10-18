//! `nullnotes://open?id=<uuid>` links (search contract v1 "open" field).
//!
//! How a link reaches us:
//! - Installed with NSIS/MSI: the installer writes HKCU/HKLM\Software\Classes\nullnotes,
//!   so Windows starts `null-notes.exe "nullnotes://open?id=…"`.
//! - Installed from the Microsoft Store (MSIX): the `windows.protocol` extension in
//!   AppxManifest.xml does the same.
//! - If Null Notes is already running, the single-instance plugin hands the link to
//!   the running copy and the new process exits (so port 47821 is never fought over).
//!
//! A link can come from any web page, so it is treated as untrusted: only
//! `open` with a well-formed uuid is accepted, and all it can do is show a note
//! that is already in one of the user's vaults.

use crate::noteid;
use crate::vault::{Vault, TEXT_KINDS};
use serde::Serialize;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

pub const SCHEME: &str = "nullnotes";

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct OpenRequest {
    /// Vault that holds the note (the frontend switches to it if needed).
    pub vault: Option<VaultRef>,
    /// Vault-relative path of the note.
    pub path: Option<String>,
    /// Set when the link couldn't be resolved; shown as a toast.
    pub error: Option<String>,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct VaultRef {
    pub path: String,
    pub name: String,
}

impl OpenRequest {
    pub fn err(msg: &str) -> Self {
        OpenRequest { vault: None, path: None, error: Some(msg.to_string()) }
    }
}

/// Accepts `nullnotes://open?id=<uuid>` (also `nullnotes:open?id=` and `nullnotes:///open?id=`).
pub fn parse(link: &str) -> Result<String, &'static str> {
    let url = tauri::Url::parse(link.trim()).map_err(|_| "That Null Notes link isn't valid.")?;
    if !url.scheme().eq_ignore_ascii_case(SCHEME) {
        return Err("That isn't a Null Notes link.");
    }
    let action = url.host_str().filter(|h| !h.is_empty()).map(|h| h.to_string()).unwrap_or_else(|| url.path().trim_matches('/').to_string());
    if !action.eq_ignore_ascii_case("open") {
        return Err("Null Notes links can only open notes.");
    }
    let id = url.query_pairs().find(|(k, _)| k == "id").map(|(_, v)| v.to_string()).ok_or("That Null Notes link has no note id.")?;
    let id = uuid::Uuid::parse_str(id.trim()).map_err(|_| "That Null Notes link has a damaged note id.")?;
    Ok(id.hyphenated().to_string())
}

/// Look through a vault for a note id without using the index: ids.json for
/// txt/html/canvas, then the frontmatter of each .md (first 4 KB only).
pub fn scan_vault(root: &Path, id: &str) -> Option<String> {
    if let Some((p, _)) = noteid::load_ids(root).into_iter().find(|(_, v)| v.eq_ignore_ascii_case(id)) {
        if root.join(&p).is_file() {
            return Some(p);
        }
    }
    let v = Vault::open(root.to_path_buf()).ok()?;
    for e in v.list_entries().ok()? {
        if e.dir || !TEXT_KINDS.contains(&e.kind.as_str()) || !noteid::is_md(&e.path) {
            continue;
        }
        let mut head = vec![0u8; 4096];
        let n = fs::File::open(v.abs(&e.path)).and_then(|mut f| f.read(&mut head)).unwrap_or(0);
        head.truncate(n);
        if noteid::get_id(&String::from_utf8_lossy(&head)).map(|x| x.eq_ignore_ascii_case(id)).unwrap_or(false) {
            return Some(e.path);
        }
    }
    None
}

fn vref(root: &Path) -> VaultRef {
    VaultRef {
        path: root.display().to_string(),
        name: root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| root.display().to_string()),
    }
}

/// Find the note: the open vault (index first, then a scan), then the user's
/// other recent vaults. Ids are uuids, so a hit in another vault is the note.
pub fn resolve(
    link: &str,
    open_vault: Option<PathBuf>,
    index_hit: impl FnOnce(&str) -> Option<(PathBuf, String)>,
    recent: &[PathBuf],
) -> OpenRequest {
    let id = match parse(link) {
        Ok(id) => id,
        Err(e) => return OpenRequest::err(e),
    };
    if let Some((root, path)) = index_hit(&id) {
        if root.join(&path).is_file() {
            return OpenRequest { vault: Some(vref(&root)), path: Some(path), error: None };
        }
    }
    let same = |a: &Path, b: &Path| a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy());
    let mut roots: Vec<&Path> = open_vault.iter().map(|p| p.as_path()).collect();
    roots.extend(recent.iter().map(|p| p.as_path()).filter(|r| !open_vault.as_deref().map(|o| same(o, r)).unwrap_or(false)));
    for root in roots {
        if !root.is_dir() {
            continue;
        }
        if let Some(path) = scan_vault(root, &id) {
            return OpenRequest { vault: Some(vref(root)), path: Some(path), error: None };
        }
    }
    OpenRequest::err("That note isn't in any of your vaults. It may have been deleted, or it's locked.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_links() {
        let id = "9fe82bd7-c8f8-424f-ae23-75466f0a05fc";
        assert_eq!(parse(&format!("nullnotes://open?id={id}")).unwrap(), id);
        assert_eq!(parse(&format!("NullNotes://OPEN/?id={}", id.to_uppercase())).unwrap(), id);
        assert_eq!(parse(&format!("nullnotes:open?id={id}")).unwrap(), id);
        assert!(parse("nullnotes://delete?id=9fe82bd7-c8f8-424f-ae23-75466f0a05fc").is_err());
        assert!(parse("nullnotes://open?id=../../etc/passwd").is_err());
        assert!(parse("nullnotes://open").is_err());
        assert!(parse("nulldrive://open?id=9fe82bd7-c8f8-424f-ae23-75466f0a05fc").is_err());
        assert!(parse("https://evil.example/?id=x").is_err());
    }

    #[test]
    fn resolve_across_vaults() {
        let base = std::env::temp_dir().join(format!("nn-links-{}", std::process::id()));
        let (a, b) = (base.join("A"), base.join("B"));
        fs::create_dir_all(a.join("x")).unwrap();
        fs::create_dir_all(b.join(".nullnotes")).unwrap();
        let id1 = "11111111-1111-4111-8111-111111111111";
        let id2 = "22222222-2222-4222-8222-222222222222";
        fs::write(a.join("x/one.md"), format!("---\nid: {id1}\n---\nhi")).unwrap();
        fs::write(b.join("two.txt"), "plain").unwrap();
        fs::write(b.join(".nullnotes/ids.json"), format!("{{\"two.txt\":\"{id2}\"}}")).unwrap();

        let r = resolve(&format!("nullnotes://open?id={id1}"), Some(a.clone()), |_| None, &[b.clone()]);
        assert_eq!(r.path.as_deref(), Some("x/one.md"));
        let r = resolve(&format!("nullnotes://open?id={id2}"), Some(a.clone()), |_| None, &[a.clone(), b.clone()]);
        assert_eq!((r.path.as_deref(), r.vault.unwrap().name.as_str()), (Some("two.txt"), "B"));
        let r = resolve("nullnotes://open?id=33333333-3333-4333-8333-333333333333", Some(a.clone()), |_| None, &[b.clone()]);
        assert!(r.error.is_some() && r.path.is_none());
        // a stale index hit (file gone) falls back to scanning
        let r = resolve(&format!("nullnotes://open?id={id1}"), Some(a.clone()), |_| Some((a.clone(), "gone.md".into())), &[]);
        assert_eq!(r.path.as_deref(), Some("x/one.md"));
        let _ = fs::remove_dir_all(&base);
    }
}
