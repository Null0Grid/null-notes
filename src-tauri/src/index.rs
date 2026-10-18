//! Local full-text index for the search agent (search contract v1).
//!
//! One SQLite file per vault, kept in the app's local data folder rather than
//! in the vault, so it never syncs and never mixes users.
//!
//! FTS5 with the trigram tokenizer is case-insensitive substring matching. On
//! its own it can't match "libre ai" to "LibreAI" because of the space, so the
//! indexed columns are a *normalised* copy of title and text: lowercase,
//! letters and digits only. "libre ai", "Libre-AI" and "LibreAI" all become
//! "libreai". Raw text is kept separately for snippets.

use crate::noteid;
use crate::vault::{kind_of, Vault, TEXT_KINDS};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

const MAX_INDEX_BYTES: u64 = 5 * 1024 * 1024;
const SNIPPET_BEFORE: usize = 50;
const SNIPPET_AFTER: usize = 110;

#[derive(Serialize, Debug, Clone)]
pub struct Hit {
    pub id: String,
    pub source: &'static str,
    pub title: String,
    pub snippet: String,
    pub path: String,
    pub open: String,
    pub modified: String,
    pub synced_id: Option<String>,
}

pub struct Index {
    db: Connection,
    root: PathBuf,
}

/// lowercase letters and digits only, with a map back to byte offsets in `s`.
fn norm_map(s: &str) -> (String, Vec<usize>) {
    let mut out = String::with_capacity(s.len());
    let mut map = Vec::with_capacity(s.len());
    for (i, c) in s.char_indices() {
        if c.is_alphanumeric() {
            for l in c.to_lowercase() {
                let before = out.len();
                out.push(l);
                for _ in before..out.len() {
                    map.push(i);
                }
            }
        }
    }
    (out, map)
}

pub fn norm(s: &str) -> String {
    norm_map(s).0
}

/// Text worth searching: markdown as-is (minus the id), HTML without tags,
/// canvas = the text of its cards and the files it points to.
/// Returns (text for snippets, extra text that is searchable but not shown).
fn searchable(rel: &str, raw: &str) -> (String, String) {
    if kind_of(rel) == Some("md") {
        // Frontmatter (tags, aliases…) is searchable; snippets show the note itself.
        let stripped = noteid::strip_id(raw);
        let (fm, body) = noteid::split_frontmatter(&stripped);
        return (body.to_string(), fm.to_string());
    }
    (searchable_plain(rel, raw), String::new())
}

fn searchable_plain(rel: &str, raw: &str) -> String {
    match kind_of(rel) {
        Some("html") => strip_html(raw),
        Some("canvas") => canvas_text(raw),
        _ => raw.to_string(),
    }
}

fn strip_html(s: &str) -> String {
    let lower = s.to_ascii_lowercase();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if lower[i..].starts_with("<script") || lower[i..].starts_with("<style") {
            let close = if lower[i..].starts_with("<script") { "</script>" } else { "</style>" };
            i = lower[i..].find(close).map(|j| i + j + close.len()).unwrap_or(s.len());
            continue;
        }
        if s.as_bytes()[i] == b'<' {
            i = s[i..].find('>').map(|j| i + j + 1).unwrap_or(s.len());
            out.push(' ');
            continue;
        }
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    let out = out.replace("&nbsp;", " ").replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'");
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn canvas_text(s: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(s) else { return String::new() };
    let mut parts = Vec::new();
    if let Some(nodes) = v.get("nodes").and_then(|n| n.as_array()) {
        for n in nodes {
            for k in ["text", "label", "file", "url"] {
                if let Some(t) = n.get(k).and_then(|t| t.as_str()) {
                    parts.push(t.to_string());
                }
            }
        }
    }
    if let Some(edges) = v.get("edges").and_then(|n| n.as_array()) {
        parts.extend(edges.iter().filter_map(|e| e.get("label").and_then(|t| t.as_str()).map(String::from)));
    }
    parts.join("\n")
}

fn title_of(rel: &str) -> String {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    match name.rfind('.') {
        Some(i) if i > 0 => name[..i].to_string(),
        _ => name.to_string(),
    }
}

fn rfc3339(ms: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp_nanos(ms as i128 * 1_000_000)
        .ok()
        .and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok())
        .unwrap_or_default()
}

/// Up to ~160 characters around the first match, on char boundaries.
fn make_snippet(body: &str, nq: &str, terms: &[String]) -> String {
    let (nb, map) = norm_map(body);
    let pos = std::iter::once(nq).chain(terms.iter().map(|s| s.as_str())).filter(|t| !t.is_empty()).find_map(|t| nb.find(t).map(|p| (p, t.len())));
    let flat = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let Some((p, len)) = pos else {
        let s = flat(body);
        return s.chars().take(SNIPPET_BEFORE + SNIPPET_AFTER).collect();
    };
    let start = map[p];
    let end = map.get(p + len - 1).map(|&e| e + body[e..].chars().next().map(|c| c.len_utf8()).unwrap_or(1)).unwrap_or(body.len());
    let pre: String = body[..start].chars().rev().take(SNIPPET_BEFORE).collect::<Vec<_>>().into_iter().rev().collect();
    let post: String = body[end..].chars().take(SNIPPET_AFTER).collect();
    let mut s = String::new();
    if pre.len() < start {
        s.push('…');
    }
    s.push_str(&flat(&format!("{pre}{}{post}", &body[start..end])));
    if end + post.len() < body.len() {
        s.push('…');
    }
    s
}

impl Index {
    pub fn open(db_path: &Path, root: PathBuf) -> rusqlite::Result<Index> {
        if let Some(d) = db_path.parent() {
            let _ = fs::create_dir_all(d);
        }
        let db = Connection::open(db_path)?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS files(
               rowid INTEGER PRIMARY KEY, path TEXT UNIQUE NOT NULL, id TEXT NOT NULL,
               mtime INTEGER NOT NULL, size INTEGER NOT NULL, title TEXT NOT NULL, body TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS files_id ON files(id);
             CREATE VIRTUAL TABLE IF NOT EXISTS fts USING fts5(ntitle, nbody, tokenize='trigram');",
        )?;
        Ok(Index { db, root })
    }

    /// Bring the index in line with the vault: index new/changed notes, drop
    /// deleted ones, and give every note a stable id (backfilling old notes).
    /// Locked notes are never indexed.
    pub fn sync(&mut self) -> rusqlite::Result<usize> {
        let Ok(vault) = Vault::open(self.root.clone()) else { return Ok(0) };
        let Ok(entries) = vault.list_entries() else { return Ok(0) };
        let notes: Vec<_> = entries.into_iter().filter(|e| !e.dir && TEXT_KINDS.contains(&e.kind.as_str())).collect();

        let mut known: HashMap<String, (i64, i64, String)> = HashMap::new();
        {
            let mut st = self.db.prepare("SELECT path, mtime, size, id FROM files")?;
            for r in st.query_map([], |r| Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?, r.get(3)?))))? {
                let (p, v) = r?;
                known.insert(p, v);
            }
        }
        let ids_json = noteid::load_ids(&self.root);
        // Paths that keep an id when two files claim the same one (a copy made in Explorer).
        let owner: HashMap<String, String> = known.iter().map(|(p, (_, _, id))| (id.clone(), p.clone())).collect();
        let present: HashSet<String> = notes.iter().map(|e| e.path.clone()).collect();

        let tx = self.db.transaction()?;
        let mut changed = 0;
        for p in known.keys().filter(|p| !present.contains(p.as_str())) {
            if let Some(rowid) = tx.query_row("SELECT rowid FROM files WHERE path=?", [p], |r| r.get::<_, i64>(0)).optional()? {
                tx.execute("DELETE FROM fts WHERE rowid=?", [rowid])?;
                tx.execute("DELETE FROM files WHERE rowid=?", [rowid])?;
                changed += 1;
            }
        }
        // Stable order so duplicate resolution is deterministic.
        let mut notes = notes;
        notes.sort_by(|a, b| a.path.cmp(&b.path));
        // Pass 1: every note's id (backfilling ones that have none).
        let mut with_ids = Vec::with_capacity(notes.len());
        for e in notes {
            let abs = vault.abs(&e.path);
            let unchanged = known.get(&e.path).map(|(m, s, _)| *m == e.modified as i64 && *s == e.size as i64).unwrap_or(false);
            let id = if noteid::is_md(&e.path) {
                if unchanged { known[&e.path].2.clone() } else { noteid::ensure_md_id(&abs).unwrap_or_default() }
            } else {
                ids_json.get(&e.path).cloned().unwrap_or_else(|| noteid::sidecar_id(&self.root, &e.path))
            };
            if !id.is_empty() {
                with_ids.push((e, abs, id, unchanged));
            }
        }
        // Pass 2: two files with one id (a copy made outside the app). The file
        // the index already knows keeps it, else the one created first.
        let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, (_, _, id, _)) in with_ids.iter().enumerate() {
            groups.entry(id.clone()).or_default().push(i);
        }
        let mut reassign: HashSet<usize> = HashSet::new();
        for (id, idxs) in groups.iter().filter(|(_, v)| v.len() > 1) {
            let created = |i: usize| fs::metadata(&with_ids[i].1).and_then(|m| m.created()).ok();
            let keep = idxs
                .iter()
                .copied()
                .find(|&i| owner.get(id) == Some(&with_ids[i].0.path))
                .unwrap_or_else(|| *idxs.iter().min_by_key(|&&i| (created(i), i)).unwrap());
            reassign.extend(idxs.iter().copied().filter(|&i| i != keep));
        }
        for (i, (e, abs, id, unchanged)) in with_ids.iter_mut().enumerate() {
            let reassigned = reassign.contains(&i);
            if reassigned {
                *id = noteid::new_id();
                if noteid::is_md(&e.path) {
                    if let Ok(b) = fs::read(&abs) {
                        noteid::set_md_id(abs, &String::from_utf8_lossy(&b), id);
                    }
                } else {
                    noteid::sidecar_set(&self.root, &e.path, id);
                }
            }
            if *unchanged && !reassigned {
                continue;
            }
            if e.size > MAX_INDEX_BYTES {
                continue;
            }
            let Ok(bytes) = fs::read(&*abs) else { continue };
            let (body, extra) = searchable(&e.path, &String::from_utf8_lossy(&bytes));
            let title = title_of(&e.path);
            // mtime after any id backfill (which preserves it anyway)
            let md = fs::metadata(&*abs).ok();
            let mtime = md.as_ref().and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_millis() as i64).unwrap_or(e.modified as i64);
            let size = md.map(|m| m.len() as i64).unwrap_or(e.size as i64);
            if let Some(rowid) = tx.query_row("SELECT rowid FROM files WHERE path=?", [&e.path], |r| r.get::<_, i64>(0)).optional()? {
                tx.execute("DELETE FROM fts WHERE rowid=?", [rowid])?;
                tx.execute("DELETE FROM files WHERE rowid=?", [rowid])?;
            }
            tx.execute(
                "INSERT INTO files(path, id, mtime, size, title, body) VALUES(?,?,?,?,?,?)",
                params![e.path, &*id, mtime, size, title, body],
            )?;
            let rowid = tx.last_insert_rowid();
            tx.execute("INSERT INTO fts(rowid, ntitle, nbody) VALUES(?,?,?)", params![rowid, norm(&title), format!("{}{}", norm(&body), norm(&extra))])?;
            changed += 1;
        }
        tx.commit()?;
        Ok(changed)
    }

    /// The app moved a file or folder: carry its rows (and ids) along, so a
    /// later copy of the old file can't claim the id.
    pub fn rename(&self, from: &str, to: &str) -> rusqlite::Result<()> {
        self.db.execute(
            "UPDATE files SET path = ?2 || substr(path, length(?1) + 1) WHERE path = ?1 OR substr(path, 1, length(?1) + 1) = ?1 || '/'",
            params![from, to],
        )?;
        Ok(())
    }

    /// Vault-relative path of the note with this id, if indexed.
    pub fn find_path(&self, id: &str) -> Option<String> {
        self.db.query_row("SELECT path FROM files WHERE id=? LIMIT 1", [id], |r| r.get(0)).optional().ok().flatten()
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn search(&self, q: &str, limit: usize) -> rusqlite::Result<Vec<Hit>> {
        let nq = norm(q);
        if nq.is_empty() {
            return Ok(vec![]);
        }
        let words: Vec<String> = q.split(|c: char| !c.is_alphanumeric()).map(norm).filter(|t| !t.is_empty()).collect();
        let terms: Vec<String> = words.iter().filter(|t| t.chars().count() >= 3).cloned().collect();
        let short: Vec<String> = words.iter().filter(|t| t.chars().count() < 3).cloned().collect();
        let mut rows: Vec<i64> = Vec::new();
        let mut seen = HashSet::new();
        // Title matches weigh 10x body matches.
        let mut run = |sql: String, args: Vec<String>| -> rusqlite::Result<Vec<i64>> {
            let mut got = Vec::new();
            let mut st = self.db.prepare(&sql)?;
            let mut p: Vec<&dyn rusqlite::ToSql> = args.iter().map(|a| a as &dyn rusqlite::ToSql).collect();
            let lim = limit as i64;
            p.push(&lim);
            for r in st.query_map(p.as_slice(), |r| r.get::<_, i64>(0))? {
                let id = r?;
                if seen.insert(id) {
                    got.push(id);
                }
            }
            Ok(got)
        };
        let like_clause = |n: usize| (0..n).map(|_| " AND (ntitle LIKE ? OR nbody LIKE ?)").collect::<String>();
        let like_args = |ws: &[String]| ws.iter().flat_map(|w| [format!("%{w}%"), format!("%{w}%")]).collect::<Vec<_>>();
        if nq.chars().count() >= 3 {
            // 1. the whole query with spaces/punctuation squeezed out: "libre ai" -> "libreai"
            rows.extend(run("SELECT rowid FROM fts WHERE fts MATCH ? ORDER BY bm25(fts, 10.0, 1.0) LIMIT ?".into(), vec![format!("\"{nq}\"")])?);
        }
        // 2. every word somewhere in the note, any order ("ai libre")
        if rows.len() < limit && words.len() > 1 {
            if terms.is_empty() {
                let sql = format!("SELECT rowid FROM fts WHERE 1{} LIMIT ?", like_clause(short.len()));
                rows.extend(run(sql, like_args(&short))?);
            } else {
                let m = terms.iter().map(|t| format!("\"{t}\"")).collect::<Vec<_>>().join(" AND ");
                let sql = format!("SELECT rowid FROM fts WHERE fts MATCH ?{} ORDER BY bm25(fts, 10.0, 1.0) LIMIT ?", like_clause(short.len()));
                let mut args = vec![m];
                args.extend(like_args(&short));
                rows.extend(run(sql, args)?);
            }
        }
        // 3. trigrams need 3+ characters; very short queries scan instead.
        if nq.chars().count() < 3 && words.len() <= 1 {
            rows.extend(run("SELECT rowid FROM fts WHERE ntitle LIKE ?1 OR nbody LIKE ?1 ORDER BY (ntitle LIKE ?1) DESC LIMIT ?2".into(), vec![format!("%{nq}%")])?);
        }
        rows.truncate(limit);
        let mut st = self.db.prepare("SELECT id, path, mtime, title, body FROM files WHERE rowid=?")?;
        let mut out = Vec::new();
        for rowid in rows {
            let r = st.query_row([rowid], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?)));
            let Ok((id, path, mtime, title, body)) = r else { continue };
            out.push(Hit {
                open: format!("nullnotes://open?id={id}"),
                snippet: make_snippet(&body, &nq, &terms),
                modified: rfc3339(mtime),
                source: "notes",
                synced_id: None,
                id,
                title,
                path,
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn norm_and_snippet() {
        assert_eq!(norm("Libre-AI notes"), "libreainotes");
        assert_eq!(norm("libre ai"), "libreai");
        let s = make_snippet("Some intro text. Then LibreAI shows up here.", "libreai", &[]);
        assert!(s.contains("LibreAI shows up"), "{s}");
        assert_eq!(strip_html("<p>Hi&amp;<b>yo</b></p><script>x()</script>"), "Hi& yo");
    }

    #[test]
    fn index_search_ids() {
        let root = std::env::temp_dir().join(format!("nn-idx-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("AI")).unwrap();
        fs::write(root.join("AI/LibreAI.md"), "# LibreAI\nSelf-hosted model notes").unwrap();
        fs::write(root.join("AI/other.md"), "---\ntags: [x]\n---\nwe use libre-ai notes daily").unwrap();
        fs::write(root.join("AI/ai libre.txt"), "unrelated words only").unwrap();
        fs::write(root.join("cooking.md"), "pasta").unwrap();
        fs::write(root.join("board.canvas"), r#"{"nodes":[{"id":"a","type":"text","text":"Libre AI roadmap"}],"edges":[]}"#).unwrap();
        let db = root.with_extension("db");
        let mut ix = Index::open(&db, root.clone()).unwrap();
        assert_eq!(ix.sync().unwrap(), 5);

        let hits = ix.search("libre ai", 20).unwrap();
        let paths: Vec<_> = hits.iter().map(|h| h.path.as_str()).collect();
        assert_eq!(paths[0], "AI/LibreAI.md", "{paths:?}");
        let o = hits.iter().find(|h| h.path == "AI/other.md").unwrap();
        assert!(!o.snippet.contains("tags") && o.snippet.contains("libre-ai notes"), "{}", o.snippet);
        assert!(ix.search("tags x", 5).is_ok());
        assert!(paths.contains(&"AI/other.md") && paths.contains(&"board.canvas") && paths.contains(&"AI/ai libre.txt"), "{paths:?}");
        assert!(!paths.contains(&"cooking.md"));
        let h = &hits[0];
        assert_eq!(h.title, "LibreAI");
        assert_eq!(h.open, format!("nullnotes://open?id={}", h.id));
        assert!(h.modified.ends_with('Z'));

        // ids: backfilled into frontmatter, other frontmatter kept, txt in ids.json
        let other = fs::read_to_string(root.join("AI/other.md")).unwrap();
        assert!(other.starts_with("---\nid: ") && other.contains("tags: [x]"), "{other}");
        let id_before = noteid::get_id(&other).unwrap();
        assert!(noteid::load_ids(&root).contains_key("AI/ai libre.txt"));

        // id survives a move, and an Explorer-style copy gets a new one
        let v = Vault::open(root.clone()).unwrap();
        v.rename_path("AI/other.md", "Moved/other.md").unwrap();
        ix.rename("AI/other.md", "Moved/other.md").unwrap();
        fs::copy(root.join("Moved/other.md"), root.join("Moved/copy.md")).unwrap();
        ix.sync().unwrap();
        let after = ix.search("daily", 10).unwrap();
        let moved = after.iter().find(|h| h.path == "Moved/other.md").unwrap();
        let copy = after.iter().find(|h| h.path == "Moved/copy.md").unwrap();
        assert_eq!(moved.id, id_before);
        assert_ne!(copy.id, id_before);

        // editor never sees the id, saving keeps it
        assert!(!v.read_text("Moved/other.md").unwrap().contains("id:"));
        v.write_text("Moved/other.md", "edited libre ai").unwrap();
        assert_eq!(noteid::get_id(&fs::read_to_string(root.join("Moved/other.md")).unwrap()).unwrap(), id_before);

        // short query and empty query
        assert!(!ix.search("ai", 10).unwrap().is_empty());
        assert!(ix.search("  ", 10).unwrap().is_empty());
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_file(&db);
    }
}
