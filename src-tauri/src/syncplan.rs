//! Two-way sync planner (Null franchise Phase 7). Pure: no disk, no network.
//!
//! Every file gets a *key* that survives renames on either side:
//!   1. its note id (`n:<uuid>`: Markdown frontmatter, `ids.json` for txt/html/canvas,
//!      Drive's `synced_id`), else
//!   2. the key it had at the last sync, found through Drive's stable file id
//!      (remote) or its old path / unchanged content (local, e.g. a renamed image),
//!   3. else its path (`p:<lowercased path>`).
//!
//! Each key is a 3-way merge of the local file, the Drive file and the
//! *base* (both sides as they were after the last successful sync):
//!
//! | local       | Drive       | result                                            |
//! |-------------|-------------|---------------------------------------------------|
//! | changed     | unchanged   | upload (only if Drive is still at the base version)|
//! | unchanged   | changed     | download (only if the local file is still at base) |
//! | changed     | changed     | **both kept**: Drive's version becomes             |
//! |             |             | "Title (conflict YYYY-MM-DD).md" (new note id),    |
//! |             |             | the local version stays and is uploaded            |
//! | deleted     | unchanged   | delete on Drive (to its 30-day trash)              |
//! | deleted     | changed     | the edit wins: downloaded again                    |
//! | unchanged   | deleted     | delete locally (to the Recycle Bin)                |
//! | changed     | deleted     | the edit wins: uploaded again                      |
//! | renamed on one side       | renamed on the other side to match         |
//! | renamed on both           | the local name wins                        |
//!
//! Two different notes that want the same path: the one already on Drive
//! keeps it, the other gets a "(conflict …)" name on both sides.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

#[derive(Debug, Clone, PartialEq)]
pub struct LItem {
    pub path: String,
    pub sha: String,
    pub note_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RItem {
    pub path: String,
    pub drive_id: String,
    pub version: i64,
    pub sha: String,
    pub note_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BItem {
    pub key: String,
    pub local_path: String,
    pub local_sha: String,
    pub drive_id: String,
    pub remote_path: String,
    pub remote_version: i64,
    pub remote_sha: String,
}

/// What a remote write may assume about Drive.
#[derive(Debug, Clone, PartialEq)]
pub enum Expect {
    /// nothing may be at the path yet
    Absent,
    /// the file must still be at this version (otherwise: retry next run)
    Version(i64),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Act {
    MoveLocal { from: String, to: String },
    MoveRemote { from: String, to: String, version: i64 },
    DeleteLocal { path: String, sha: String },
    DeleteRemote { path: String, version: i64 },
    /// Save Drive's version of `remote` as a new local file `copy` (new note
    /// id) and upload that copy as a new Drive file at the same path.
    ConflictCopy { remote: String, version: i64, copy: String },
    /// `expect_local`: the local file's hash it must still have (None: must not exist).
    Download { remote: String, version: i64, local: String, expect_local: Option<String> },
    Upload { local: String, remote: String, expect: Expect },
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Plan {
    pub acts: Vec<Act>,
    /// conflict copies that will be created (local paths), for the user
    pub conflicts: Vec<String>,
    pub problems: Vec<String>,
}

fn lower(p: &str) -> String {
    p.to_lowercase()
}

/// Match local files, Drive files and the base by key (see module docs).
pub fn keys<'a>(local: &'a [LItem], remote: &'a [RItem], base: &[BItem]) -> (BTreeMap<String, &'a LItem>, BTreeMap<String, &'a RItem>) {
    let base_by_key: HashMap<&str, &BItem> = base.iter().map(|b| (b.key.as_str(), b)).collect();
    let local_paths: HashSet<String> = local.iter().map(|l| lower(&l.path)).collect();
    let mut lkey: BTreeMap<String, &LItem> = BTreeMap::new();
    let mut used_moves: HashSet<&str> = HashSet::new();
    for l in local {
        let k = if let Some(id) = &l.note_id {
            format!("n:{}", id.to_lowercase())
        } else if let Some(b) = base.iter().find(|b| !b.key.starts_with("n:") && lower(&b.local_path) == lower(&l.path)) {
            b.key.clone()
        } else if let Some(b) = base.iter().find(|b| {
            !b.key.starts_with("n:") && !local_paths.contains(&lower(&b.local_path)) && b.local_sha == l.sha && !used_moves.contains(b.key.as_str())
        }) {
            // a keyless file (image, locked note) renamed outside the app
            used_moves.insert(&b.key);
            b.key.clone()
        } else {
            format!("p:{}", lower(&l.path))
        };
        lkey.entry(k).or_insert(l);
    }
    let mut rkey: BTreeMap<String, &RItem> = BTreeMap::new();
    for r in remote {
        let k = if let Some(id) = &r.note_id {
            format!("n:{}", id.to_lowercase())
        } else if let Some(b) = base.iter().find(|b| b.drive_id == r.drive_id) {
            b.key.clone()
        } else {
            format!("p:{}", lower(&r.path))
        };
        rkey.entry(k).or_insert(r);
    }
    // Same path, same content, different keys (e.g. a note whose id is only
    // known on one side): they are the same file.
    let lonly: Vec<String> = lkey.keys().filter(|k| !rkey.contains_key(*k) && !base_by_key.contains_key(k.as_str())).cloned().collect();
    for lk in lonly {
        let l = lkey[&lk];
        if let Some(rk) = rkey.iter().find(|(rk, r)| !lkey.contains_key(*rk) && !base_by_key.contains_key(rk.as_str()) && lower(&r.path) == lower(&l.path) && r.sha == l.sha).map(|(k, _)| k.clone()) {
            let r = rkey.remove(&rk).unwrap();
            rkey.insert(lk, r);
        }
    }

    (lkey, rkey)
}

/// `conflict_name(path, taken)` must return a path not in `taken` (lowercased).
pub fn plan(
    local: &[LItem],
    remote: &[RItem],
    base: &[BItem],
    conflict_name: &dyn Fn(&str, &HashSet<String>) -> String,
    allow_mass_delete: bool,
) -> Plan {
    let mut out = Plan::default();
    let base_by_key: HashMap<&str, &BItem> = base.iter().map(|b| (b.key.as_str(), b)).collect();

    let (lkey, rkey) = keys(local, remote, base);

    /* ---- per-key decision ---- */
    #[derive(Debug)]
    enum C {
        Nothing,
        Upload,
        Download,
        Conflict,
        DelLocal,
        DelRemote,
    }
    struct D<'a> {
        l: Option<&'a LItem>,
        r: Option<&'a RItem>,
        c: C,
        fin: String, // final path (both sides)
    }
    let keys: BTreeSet<String> = lkey.keys().chain(rkey.keys()).chain(base.iter().map(|b| &b.key)).cloned().collect();
    let mut ds: Vec<D> = Vec::new();
    for k in keys {
        let (l, r, b) = (lkey.get(&k).copied(), rkey.get(&k).copied(), base_by_key.get(k.as_str()).copied());
        let d = match (l, r, b) {
            (None, None, _) => continue,
            (Some(l), None, None) => D { l: Some(l), r: None, c: C::Upload, fin: l.path.clone() },
            (None, Some(r), None) => D { l: None, r: Some(r), c: C::Download, fin: r.path.clone() },
            (Some(l), Some(r), None) => D { l: Some(l), r: Some(r), c: if l.sha == r.sha { C::Nothing } else { C::Conflict }, fin: l.path.clone() },
            (Some(l), None, Some(b)) => {
                let lc = l.sha != b.local_sha;
                D { l: Some(l), r: None, c: if lc { C::Upload } else { C::DelLocal }, fin: l.path.clone() }
            }
            (None, Some(r), Some(b)) => {
                let rc = r.sha != b.remote_sha;
                D { l: None, r: Some(r), c: if rc { C::Download } else { C::DelRemote }, fin: r.path.clone() }
            }
            (Some(l), Some(r), Some(b)) => {
                let lc = l.sha != b.local_sha;
                let rc = r.sha != b.remote_sha;
                let lm = lower(&l.path) != lower(&b.local_path) || l.path != b.local_path;
                let rm = r.path != b.remote_path;
                let fin = if rm && !lm { r.path.clone() } else { l.path.clone() };
                let c = match (lc, rc) {
                    (false, false) => C::Nothing,
                    (true, false) => C::Upload,
                    (false, true) => C::Download,
                    (true, true) => {
                        if l.sha == r.sha {
                            C::Nothing
                        } else {
                            C::Conflict
                        }
                    }
                };
                D { l: Some(l), r: Some(r), c, fin }
            }
        };
        ds.push(d);
    }

    /* ---- deletion guards ---- */
    let del_local = ds.iter().filter(|d| matches!(d.c, C::DelLocal)).count();
    let del_remote = ds.iter().filter(|d| matches!(d.c, C::DelRemote)).count();
    let mass = |n: usize| n > 20 && n * 2 > base.len();
    let mut block_local_del = false;
    let mut block_remote_del = false;
    if del_local > 0 && remote.is_empty() && !base.is_empty() {
        block_local_del = true;
        out.problems.push("The Drive folder is empty, so nothing was deleted here. If you emptied it on purpose, delete the notes in the app.".into());
    } else if del_local > 0 && mass(del_local) && !allow_mass_delete {
        block_local_del = true;
        out.problems.push(format!("{del_local} notes were deleted on Drive. To be safe they weren't deleted here yet: press Sync now in Settings to confirm."));
    }
    if del_remote > 0 && local.is_empty() && !base.is_empty() {
        block_remote_del = true;
        out.problems.push("The vault looks empty, so nothing was deleted from Drive.".into());
    } else if del_remote > 0 && mass(del_remote) && !allow_mass_delete {
        block_remote_del = true;
        out.problems.push(format!("{del_remote} notes were deleted here. To be safe they weren't deleted on Drive yet: press Sync now in Settings to confirm."));
    }
    for d in ds.iter_mut() {
        if (matches!(d.c, C::DelLocal) && block_local_del) || (matches!(d.c, C::DelRemote) && block_remote_del) {
            d.c = C::Nothing;
        }
    }

    /* ---- path claims: two notes, one path ---- */
    let claims = |d: &D| !matches!(d.c, C::DelLocal | C::DelRemote);
    let mut by_path: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, d) in ds.iter().enumerate() {
        if claims(d) {
            by_path.entry(lower(&d.fin)).or_default().push(i);
        }
    }
    let mut taken: HashSet<String> = by_path.keys().cloned().collect();
    // paths that are deleted this run are free again, everything else on disk/Drive is taken
    for d in ds.iter().filter(|d| !claims(d)) {
        if let Some(l) = d.l {
            taken.remove(&lower(&l.path));
        }
        if let Some(r) = d.r {
            taken.remove(&lower(&r.path));
        }
    }
    for (_, idx) in by_path.iter().filter(|(_, v)| v.len() > 1) {
        let at = |i: usize, f: &dyn Fn(&D) -> bool| f(&ds[i]);
        let keep = idx
            .iter()
            .copied()
            .find(|&i| at(i, &|d| d.r.map(|r| lower(&r.path) == lower(&d.fin)).unwrap_or(false) && d.l.map(|l| lower(&l.path) == lower(&d.fin)).unwrap_or(false)))
            .or_else(|| idx.iter().copied().find(|&i| at(i, &|d| d.r.map(|r| lower(&r.path) == lower(&d.fin)).unwrap_or(false))))
            .or_else(|| idx.iter().copied().find(|&i| at(i, &|d| d.l.map(|l| lower(&l.path) == lower(&d.fin)).unwrap_or(false))))
            .unwrap_or(idx[0]);
        for &i in idx.iter().filter(|&&i| i != keep) {
            let n = conflict_name(&ds[i].fin, &taken);
            taken.insert(lower(&n));
            out.conflicts.push(n.clone());
            ds[i].fin = n;
        }
    }

    /* ---- actions, in execution order ---- */
    let mut moves_l = vec![];
    let mut moves_r = vec![];
    let mut dels = vec![];
    let mut copies = vec![];
    let mut downs = vec![];
    let mut ups = vec![];
    for d in &ds {
        match d.c {
            C::DelLocal => {
                let l = d.l.unwrap();
                dels.push(Act::DeleteLocal { path: l.path.clone(), sha: l.sha.clone() });
                continue;
            }
            C::DelRemote => {
                let r = d.r.unwrap();
                dels.push(Act::DeleteRemote { path: r.path.clone(), version: r.version });
                continue;
            }
            _ => {}
        }
        if let Some(l) = d.l {
            if l.path != d.fin {
                moves_l.push(Act::MoveLocal { from: l.path.clone(), to: d.fin.clone() });
            }
        }
        if let Some(r) = d.r {
            if r.path != d.fin {
                moves_r.push(Act::MoveRemote { from: r.path.clone(), to: d.fin.clone(), version: r.version });
            }
        }
        match d.c {
            C::Nothing | C::DelLocal | C::DelRemote => {}
            C::Upload => ups.push(Act::Upload {
                local: d.fin.clone(),
                remote: d.fin.clone(),
                expect: d.r.map(|r| Expect::Version(r.version)).unwrap_or(Expect::Absent),
            }),
            C::Download => downs.push(Act::Download {
                remote: d.fin.clone(),
                version: d.r.unwrap().version,
                local: d.fin.clone(),
                expect_local: d.l.map(|l| l.sha.clone()),
            }),
            C::Conflict => {
                let r = d.r.unwrap();
                let copy = conflict_name(&d.fin, &taken);
                taken.insert(lower(&copy));
                out.conflicts.push(copy.clone());
                copies.push(Act::ConflictCopy { remote: d.fin.clone(), version: r.version, copy });
                ups.push(Act::Upload { local: d.fin.clone(), remote: d.fin.clone(), expect: Expect::Version(r.version) });
            }
        }
    }
    out.acts.extend(moves_l);
    out.acts.extend(moves_r);
    out.acts.extend(dels);
    out.acts.extend(copies);
    out.acts.extend(downs);
    out.acts.extend(ups);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cn(p: &str, taken: &HashSet<String>) -> String {
        let (stem, ext) = p.rsplit_once('.').map(|(s, e)| (s.to_string(), format!(".{e}"))).unwrap_or((p.to_string(), String::new()));
        let first = format!("{stem} (conflict 2026-10-18){ext}");
        if !taken.contains(&first.to_lowercase()) {
            return first;
        }
        (2..).map(|n| format!("{stem} (conflict 2026-10-18 {n}){ext}")).find(|x| !taken.contains(&x.to_lowercase())).unwrap()
    }
    fn l(p: &str, sha: &str, id: Option<&str>) -> LItem {
        LItem { path: p.into(), sha: sha.into(), note_id: id.map(String::from) }
    }
    fn r(p: &str, did: &str, v: i64, sha: &str, id: Option<&str>) -> RItem {
        RItem { path: p.into(), drive_id: did.into(), version: v, sha: sha.into(), note_id: id.map(String::from) }
    }
    fn b(key: &str, lp: &str, did: &str, rp: &str, v: i64, sha: &str) -> BItem {
        BItem { key: key.into(), local_path: lp.into(), local_sha: sha.into(), drive_id: did.into(), remote_path: rp.into(), remote_version: v, remote_sha: sha.into() }
    }
    fn p(lo: &[LItem], re: &[RItem], ba: &[BItem]) -> Plan {
        plan(lo, re, ba, &cn, false)
    }

    #[test]
    fn in_sync_does_nothing() {
        let pl = p(&[l("a.md", "1", Some("A")), l("pic.png", "9", None)], &[r("a.md", "d1", 3, "1", Some("A")), r("pic.png", "d2", 1, "9", None)], &[]);
        assert_eq!(pl, Plan::default(), "first sync of identical sides");
        let base = [b("n:a", "a.md", "d1", "a.md", 3, "1"), b("p:pic.png", "pic.png", "d2", "pic.png", 1, "9")];
        assert_eq!(p(&[l("a.md", "1", Some("A")), l("pic.png", "9", None)], &[r("a.md", "d1", 3, "1", Some("A")), r("pic.png", "d2", 1, "9", None)], &base), Plan::default());
    }

    #[test]
    fn one_sided_edits() {
        let base = [b("n:a", "a.md", "d1", "a.md", 3, "1")];
        let up = p(&[l("a.md", "2", Some("A"))], &[r("a.md", "d1", 3, "1", Some("A"))], &base);
        assert_eq!(up.acts, vec![Act::Upload { local: "a.md".into(), remote: "a.md".into(), expect: Expect::Version(3) }]);
        let down = p(&[l("a.md", "1", Some("A"))], &[r("a.md", "d1", 4, "2", Some("A"))], &base);
        assert_eq!(down.acts, vec![Act::Download { remote: "a.md".into(), version: 4, local: "a.md".into(), expect_local: Some("1".into()) }]);
    }

    #[test]
    fn both_edited_keeps_both() {
        let base = [b("n:a", "Notes/Plan.md", "d1", "Notes/Plan.md", 3, "1")];
        let pl = p(&[l("Notes/Plan.md", "2", Some("A"))], &[r("Notes/Plan.md", "d1", 4, "3", Some("A"))], &base);
        assert_eq!(pl.conflicts, vec!["Notes/Plan (conflict 2026-10-18).md".to_string()]);
        assert_eq!(pl.acts, vec![
            Act::ConflictCopy { remote: "Notes/Plan.md".into(), version: 4, copy: "Notes/Plan (conflict 2026-10-18).md".into() },
            Act::Upload { local: "Notes/Plan.md".into(), remote: "Notes/Plan.md".into(), expect: Expect::Version(4) },
        ]);
        // both made the same edit: nothing to do
        assert!(p(&[l("Notes/Plan.md", "2", Some("A"))], &[r("Notes/Plan.md", "d1", 4, "2", Some("A"))], &base).acts.is_empty());
        // first sync, same note, different content: also both kept
        let first = p(&[l("x.md", "2", Some("X"))], &[r("x.md", "dx", 1, "3", Some("X"))], &[]);
        assert_eq!(first.conflicts.len(), 1);
    }

    #[test]
    fn renames_matched_by_note_id() {
        let base = [b("n:a", "a.md", "d1", "a.md", 3, "1")];
        // renamed locally (Explorer or app): moved on Drive, not re-uploaded
        let pl = p(&[l("Archive/a2.md", "1", Some("A"))], &[r("a.md", "d1", 3, "1", Some("A"))], &base);
        assert_eq!(pl.acts, vec![Act::MoveRemote { from: "a.md".into(), to: "Archive/a2.md".into(), version: 3 }]);
        // renamed on Drive (another device): renamed here
        let pl = p(&[l("a.md", "1", Some("A"))], &[r("b.md", "d1", 4, "1", Some("A"))], &base);
        assert_eq!(pl.acts, vec![Act::MoveLocal { from: "a.md".into(), to: "b.md".into() }]);
        // renamed on Drive + edited there, while the local copy was untouched
        let pl = p(&[l("a.md", "1", Some("A"))], &[r("b.md", "d1", 5, "2", Some("A"))], &base);
        assert_eq!(pl.acts, vec![
            Act::MoveLocal { from: "a.md".into(), to: "b.md".into() },
            Act::Download { remote: "b.md".into(), version: 5, local: "b.md".into(), expect_local: Some("1".into()) },
        ]);
        // renamed differently on both: the local name wins
        let pl = p(&[l("mine.md", "1", Some("A"))], &[r("theirs.md", "d1", 4, "1", Some("A"))], &base);
        assert_eq!(pl.acts, vec![Act::MoveRemote { from: "theirs.md".into(), to: "mine.md".into(), version: 4 }]);
        // renamed locally + edited on Drive: move Drive's file, then download
        let pl = p(&[l("new.md", "1", Some("A"))], &[r("a.md", "d1", 4, "2", Some("A"))], &base);
        assert_eq!(pl.acts, vec![
            Act::MoveRemote { from: "a.md".into(), to: "new.md".into(), version: 4 },
            Act::Download { remote: "new.md".into(), version: 4, local: "new.md".into(), expect_local: Some("1".into()) },
        ]);
    }

    #[test]
    fn keyless_files_follow_drive_id_and_content() {
        let base = [b("p:pic.png", "pic.png", "d2", "pic.png", 1, "9")];
        // image renamed in Explorer: same bytes at a new path
        let pl = p(&[l("img/pic2.png", "9", None)], &[r("pic.png", "d2", 1, "9", None)], &base);
        assert_eq!(pl.acts, vec![Act::MoveRemote { from: "pic.png".into(), to: "img/pic2.png".into(), version: 1 }]);
        // image renamed on Drive: Drive's file id keeps the key
        let pl = p(&[l("pic.png", "9", None)], &[r("x/pic.png", "d2", 2, "9", None)], &base);
        assert_eq!(pl.acts, vec![Act::MoveLocal { from: "pic.png".into(), to: "x/pic.png".into() }]);
    }

    #[test]
    fn deletes_and_edit_wins_over_delete() {
        let base = [b("n:a", "a.md", "d1", "a.md", 3, "1")];
        let other = b("n:k", "keep.md", "d9", "keep.md", 1, "k");
        let lk = l("keep.md", "k", Some("K"));
        let rk = r("keep.md", "d9", 1, "k", Some("K"));
        let pl = p(&[lk.clone()], &[r("a.md", "d1", 3, "1", Some("A")), rk.clone()], &[base[0].clone(), other.clone()]);
        assert_eq!(pl.acts, vec![Act::DeleteRemote { path: "a.md".into(), version: 3 }]);
        let pl = p(&[lk.clone()], &[r("a.md", "d1", 4, "2", Some("A")), rk.clone()], &[base[0].clone(), other.clone()]);
        assert_eq!(pl.acts, vec![Act::Download { remote: "a.md".into(), version: 4, local: "a.md".into(), expect_local: None }], "edited on Drive after the local delete: comes back");
        let pl = p(&[l("a.md", "1", Some("A")), lk.clone()], &[rk.clone()], &[base[0].clone(), other.clone()]);
        assert_eq!(pl.acts, vec![Act::DeleteLocal { path: "a.md".into(), sha: "1".into() }]);
        let pl = p(&[l("a.md", "2", Some("A")), lk.clone()], &[rk.clone()], &[base[0].clone(), other.clone()]);
        assert_eq!(pl.acts, vec![Act::Upload { local: "a.md".into(), remote: "a.md".into(), expect: Expect::Absent }], "edited here after the Drive delete: goes back up");
    }

    #[test]
    fn empty_side_and_mass_delete_guards() {
        let base: Vec<BItem> = (0..30).map(|i| b(&format!("n:{i}"), &format!("{i}.md"), &format!("d{i}"), &format!("{i}.md"), 1, "s")).collect();
        let loc: Vec<LItem> = (0..30).map(|i| l(&format!("{i}.md"), "s", Some(&i.to_string()))).collect();
        let rem: Vec<RItem> = (0..30).map(|i| r(&format!("{i}.md"), &format!("d{i}"), 1, "s", Some(&i.to_string()))).collect();
        // Drive folder emptied / wrong folder: nothing deleted locally
        let pl = p(&loc, &[], &base);
        assert!(pl.acts.is_empty() && !pl.problems.is_empty());
        // vault empty (unplugged disk): nothing deleted on Drive
        let pl = p(&[], &rem, &base);
        assert!(pl.acts.is_empty() && !pl.problems.is_empty());
        // 25 of 30 deleted on Drive: held back until confirmed
        let pl = p(&loc, &rem[25..], &base);
        assert!(pl.acts.is_empty() && pl.problems[0].contains("25"));
        let pl = plan(&loc, &rem[25..], &base, &cn, true);
        assert_eq!(pl.acts.len(), 25);
    }

    #[test]
    fn two_new_notes_same_name() {
        // made offline on two devices: Drive's keeps the name, ours is renamed on both sides
        let pl = p(&[l("Ideas.md", "1", Some("A"))], &[r("Ideas.md", "d5", 1, "2", Some("B"))], &[]);
        assert_eq!(pl.conflicts, vec!["Ideas (conflict 2026-10-18).md".to_string()]);
        assert_eq!(pl.acts, vec![
            Act::MoveLocal { from: "Ideas.md".into(), to: "Ideas (conflict 2026-10-18).md".into() },
            Act::Download { remote: "Ideas.md".into(), version: 1, local: "Ideas.md".into(), expect_local: None },
            Act::Upload { local: "Ideas (conflict 2026-10-18).md".into(), remote: "Ideas (conflict 2026-10-18).md".into(), expect: Expect::Absent },
        ]);
        // Windows names are case-insensitive
        let pl = p(&[l("ideas.md", "1", Some("A"))], &[r("Ideas.md", "d5", 1, "2", Some("B"))], &[]);
        assert_eq!(pl.conflicts.len(), 1);
        // a new local note can't take the path of a note renamed away on Drive in the same run? it can:
        let base = [b("n:x", "a.md", "dx", "a.md", 1, "1")];
        let pl = p(&[l("a.md", "1", Some("X"))], &[r("old/a.md", "dx", 2, "1", Some("X")), r("a.md", "dy", 1, "7", Some("Y"))], &base);
        assert!(pl.conflicts.is_empty(), "{pl:?}");
        assert_eq!(pl.acts, vec![
            Act::MoveLocal { from: "a.md".into(), to: "old/a.md".into() },
            Act::Download { remote: "a.md".into(), version: 1, local: "a.md".into(), expect_local: None },
        ]);
    }
}
