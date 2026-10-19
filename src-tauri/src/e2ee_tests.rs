//! Phase 8 end to end: Null Notes against an encrypted Drive (the mock in
//! `e2ee_mock`, which has no key, like the real server).

use crate::backup::{self, Drive};
use crate::e2ee::{self, Keys, Secret};
use crate::e2ee_mock::{self, PASSPHRASE, RECOVERY};
use crate::noteid;
use crate::sync::{run_sync, DriveRemote, SyncReport, SyncState};
use crate::vault::Vault;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

const FOLDER: &str = "Null Notes/Work";

fn bundle() -> e2ee::Bundle {
    let v: serde_json::Value = serde_json::from_str(e2ee_mock::BUNDLE).unwrap();
    serde_json::from_value(v["bundle"].clone()).unwrap()
}

fn keys() -> Arc<Keys> {
    Arc::new(e2ee::unlock(&bundle(), &Secret::Passphrase(PASSPHRASE)).unwrap())
}

fn tmp(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("nn-e2ee-{}-{}-{}", std::process::id(), name, backup::now_rfc3339().replace(':', "")));
    fs::create_dir_all(&p).unwrap();
    p
}

struct Dev {
    root: PathBuf,
    st: SyncState,
    drive: Drive,
}

impl Dev {
    fn new(name: &str, url: &str) -> Dev {
        let mut drive = Drive::new(url, "tok").unwrap();
        drive.backoff = vec![];
        drive.keys = Some(keys());
        Dev { root: tmp(name), st: SyncState::default(), drive }
    }
    fn sync(&mut self) -> SyncReport {
        let r = DriveRemote::new(&self.drive, FOLDER.into());
        let mut out = run_sync(&self.root, &r, &mut self.st, false).unwrap();
        for _ in 0..3 {
            if !out.retry {
                break;
            }
            let more = run_sync(&self.root, &r, &mut self.st, false).unwrap();
            out = SyncReport { conflicts: [out.conflicts, more.conflicts.clone()].concat(), ..more };
        }
        out
    }
    fn v(&self) -> Vault {
        Vault::open(self.root.clone()).unwrap()
    }
    fn read(&self, p: &str) -> String {
        self.v().read_text(p).unwrap()
    }
    fn files(&self) -> Vec<String> {
        let mut v: Vec<String> = self.v().list_entries().unwrap().into_iter().filter(|e| !e.dir).map(|e| e.path).collect();
        v.sort();
        v
    }
    fn id_of(&self, p: &str) -> String {
        noteid::get_id(&fs::read_to_string(self.root.join(p)).unwrap()).unwrap()
    }
}

impl Drop for Dev {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn later() {
    std::thread::sleep(std::time::Duration::from_millis(15));
}

/// What the mock holds, decrypted with the account key (only the test has it).
fn drive_view(m: &e2ee_mock::Mock) -> Vec<(String, String)> {
    let k = keys();
    let g = m.s.lock().unwrap();
    let mut out: Vec<(String, String)> = g.files.iter().map(|(p, f)| (k.decrypt_path(p).unwrap(), f.id.clone())).collect();
    out.sort();
    out
}

fn assert_no_plaintext(m: &e2ee_mock::Mock, secrets: &[&str]) {
    let g = m.s.lock().unwrap();
    let wire = String::from_utf8_lossy(&g.wire);
    for s in secrets {
        assert!(!wire.contains(s), "{s:?} reached the server in plaintext");
    }
    assert!(g.refusals.is_empty(), "the server refused requests: {:?}", g.refusals);
    assert!(!g.log.iter().any(|l| l.starts_with("PUT /api/v1/files/") || l.starts_with("GET /api/v1/files/")), "v1 whole-file calls used: {:?}", g.log);
}

#[test]
fn e2ee_sync_create_edit_conflict_move_delete() {
    let m = e2ee_mock::start("tok");
    let (mut a, mut b) = (Dev::new("a", &m.url), Dev::new("b", &m.url));

    // create
    a.v().write_text("Plan.md", "Ship the encrypted sync.").unwrap();
    a.v().write_text("Ideas.md", "Rack shelf").unwrap();
    a.v().write_text("Trash me.md", "bye").unwrap();
    a.v().write_text("Inbox/todo.txt", "[ ] label cables").unwrap();
    a.v().write_text("Empty.md", "").unwrap();
    fs::write(a.root.join("pic.png"), b"\x89PNG secret pixels").unwrap();
    let ra = a.sync();
    assert!(ra.problems.is_empty(), "{ra:?}");
    let rb = b.sync();
    assert!(rb.problems.is_empty(), "{rb:?}");
    assert_eq!(a.files(), b.files());
    assert_eq!(b.read("Plan.md"), "Ship the encrypted sync.");
    assert_eq!(a.id_of("Plan.md"), b.id_of("Plan.md"), "the note id came along in the frontmatter");
    assert_eq!(noteid::load_ids(&a.root).get("Inbox/todo.txt"), noteid::load_ids(&b.root).get("Inbox/todo.txt"), "txt ids via the (encrypted) ids.json");
    assert!(drive_view(&m).iter().any(|(p, _)| p == "Null Notes/Work/.nullnotes/ids.json"));
    let plan_id = a.id_of("Plan.md");

    // nothing changed: nothing is uploaded again (keyed hashes compare equal)
    let before = m.s.lock().unwrap().log.len();
    let r = a.sync();
    let r2 = b.sync();
    assert!(r.uploaded.is_empty() && r.downloaded.is_empty() && r2.uploaded.is_empty() && r2.downloaded.is_empty(), "{r:?} {r2:?}");
    assert!(!m.s.lock().unwrap().log[before..].iter().any(|l| l.starts_with("PUT")), "no writes when nothing changed");

    // edit on one side
    later();
    a.v().write_text("Ideas.md", "Rack shelf, 10 inch").unwrap();
    a.sync();
    let r = b.sync();
    assert_eq!(r.downloaded, vec!["Ideas.md".to_string()]);
    assert_eq!(b.read("Ideas.md"), "Rack shelf, 10 inch");

    // conflict: both edit offline -> both kept, never overwritten
    later();
    a.v().write_text("Plan.md", "from A").unwrap();
    b.v().write_text("Plan.md", "from B").unwrap();
    // move (Explorer rename on A, of a md and of the image) and delete (on B)
    fs::create_dir_all(a.root.join("Archive")).unwrap();
    let ideas_drive_id = drive_view(&m).into_iter().find(|(p, _)| p.ends_with("/Ideas.md")).unwrap().1;
    fs::rename(a.root.join("Ideas.md"), a.root.join("Archive/Ideas.md")).unwrap();
    fs::create_dir_all(a.root.join("img")).unwrap();
    fs::rename(a.root.join("pic.png"), a.root.join("img/pic.png")).unwrap();
    fs::remove_file(b.root.join("Trash me.md")).unwrap();

    let ra = a.sync();
    let rb = b.sync();
    let ra2 = a.sync();
    assert!(ra.conflicts.is_empty() && ra.problems.is_empty() && ra2.problems.is_empty(), "{ra:?} {ra2:?}");
    assert_eq!(rb.conflicts.len(), 1, "{rb:?}");
    assert_eq!(a.files(), b.files(), "both devices end the same");
    let texts: Vec<String> = a.files().iter().filter(|p| p.ends_with(".md")).map(|p| a.read(p)).collect();
    assert!(texts.contains(&"from A".into()) && texts.contains(&"from B".into()), "{texts:?}");
    assert_eq!(a.id_of("Plan.md"), plan_id, "the original keeps its id; the copy got a new one");
    let copy = a.files().into_iter().find(|p| p.contains("(conflict ")).unwrap();
    assert_ne!(a.id_of(&copy), plan_id);
    assert!(b.files().contains(&"Archive/Ideas.md".to_string()) && !b.files().contains(&"Ideas.md".to_string()));
    assert!(b.files().contains(&"img/pic.png".to_string()));
    assert!(!a.files().contains(&"Trash me.md".to_string()), "the delete reached A");
    let view = drive_view(&m);
    let moved = view.iter().find(|(p, _)| p == "Null Notes/Work/Archive/Ideas.md").unwrap();
    assert_eq!(moved.1, ideas_drive_id, "a rename is a Drive move: same file id, history kept");
    {
        let g = m.s.lock().unwrap();
        assert!(g.log.iter().any(|l| l.starts_with("POST /api/v1/files/move")));
        let k = keys();
        let trashed: Vec<String> = g.trash.iter().map(|p| k.decrypt_path(p).unwrap()).collect();
        assert_eq!(trashed, vec!["Null Notes/Work/Trash me.md".to_string()], "deleted into Drive's trash, nothing else");
    }

    // a write that races another device's: never overwritten, both kept
    later();
    a.v().write_text("Plan.md", "A again").unwrap();
    let k = keys();
    let enc_plan = k.encrypt_path("Null Notes/Work/Plan.md").unwrap();
    let c_text = noteid::with_id("C's edit", &plan_id);
    let parts = k.seal_content(c_text.as_bytes());
    let c_sha = k.file_hash(c_text.as_bytes());
    m.s.lock().unwrap().hook = Some(Box::new(move |g: &mut e2ee_mock::S| {
        for (h, s) in &parts {
            g.chunks.insert(h.clone(), s.clone());
        }
        let mut f = g.files[&enc_plan].clone();
        f.version = g.bump();
        f.size = c_text.len() as u64;
        f.sha256 = c_sha.clone();
        f.chunks = parts.iter().map(|(h, s)| (h.clone(), s.len() as u64)).collect();
        g.files.insert(enc_plan.clone(), f);
    }));
    let r = a.sync();
    assert_eq!(r.conflicts.len(), 1, "{r:?}");
    let texts: Vec<String> = a.files().iter().filter(|p| p.ends_with(".md")).map(|p| a.read(p)).collect();
    assert!(texts.contains(&"A again".into()) && texts.contains(&"C's edit".into()), "{texts:?}");
    b.sync();
    assert_eq!(a.files(), b.files());

    // the server never saw a name, a note's text or a note id
    assert_no_plaintext(&m, &["Plan", "Ideas", "Inbox", "todo", "Archive", "Null Notes", "Ship the encrypted", "from A", "label cables", "secret pixels", &plan_id]);
    assert!(!String::from_utf8_lossy(&m.s.lock().unwrap().wire).contains("synced_id"));
}

#[test]
fn e2ee_rows_that_dont_decrypt_are_skipped_and_never_touched() {
    let m = e2ee_mock::start("tok");
    let other = Keys::from_master(&[9u8; 32]).unwrap();
    m.s.lock().unwrap().junk_rows = vec![
        serde_json::json!({"id": "f1", "path": other.encrypt_name("Someone else.md").unwrap(), "is_dir": false, "size": 3, "sha256": "0".repeat(64), "version": 7, "deleted": false}),
    ];
    let mut a = Dev::new("a", &m.url);
    a.v().write_text("Mine.md", "x").unwrap();
    let r = a.sync();
    assert_eq!(r.problems.len(), 1, "reported: {r:?}");
    assert!(r.problems[0].contains("1 file in the Drive folder couldn't be decrypted"), "{r:?}");
    assert_eq!(a.files(), vec!["Mine.md".to_string()], "the undecryptable row isn't downloaded");
    let r = a.sync();
    assert!(r.deleted_remote.is_empty() && r.downloaded.is_empty());
    let (rows, skipped) = a.drive.list_rows(FOLDER).unwrap();
    assert_eq!(skipped, 1);
    assert!(rows.iter().all(|r| !r.path.contains('~')));
}

#[test]
fn e2ee_refusals_match_drive() {
    let m = e2ee_mock::start("tok");
    // a plain client against an encrypted account fails safely
    let mut plain = Drive::new(&m.url, "tok").unwrap();
    plain.backoff = vec![];
    assert!(matches!(plain.list("Null Notes"), Err(backup::DErr::Other(_))), "listing without the header");
    let root = tmp("plainrun");
    Vault::open(root.clone()).unwrap().write_text("a.md", "x").unwrap();
    let mut st = backup::State::default();
    assert_eq!(backup::run_once(&root, FOLDER, &plain, &mut st).unwrap_err(), backup::DErr::E2ee, "backup asks to unlock instead of uploading plaintext");
    let _ = fs::remove_dir_all(&root);
    // names Windows can't hold are refused before anything is sent
    let mut d = Drive::new(&m.url, "tok").unwrap();
    d.backoff = vec![];
    d.keys = Some(keys());
    assert!(matches!(d.commit_encrypted("Null Notes/Work/CON.md", b"x", &backup::Precond::Absent), Err(backup::DErr::File(_))));
    // create-only twice: the second is a conflict; an identical retry is fine
    d.commit_encrypted("Null Notes/Work/n.md", b"one", &backup::Precond::Absent).unwrap();
    assert!(matches!(d.commit_encrypted("Null Notes/Work/n.md", b"two", &backup::Precond::Absent), Err(backup::DErr::Precondition(_))));
    d.commit_encrypted("Null Notes/Work/n.md", b"one", &backup::Precond::Absent).unwrap();
    assert_eq!(d.download_encrypted("Null Notes/Work/n.md").unwrap(), b"one");
    // empty file: no chunks
    d.commit_encrypted("Null Notes/Work/e.md", b"", &backup::Precond::Absent).unwrap();
    assert_eq!(d.download_encrypted("Null Notes/Work/e.md").unwrap(), b"");
    // multi-chunk
    let big: Vec<u8> = (0..(e2ee::PIECE + 1000)).map(|i| (i % 251) as u8).collect();
    d.commit_encrypted("Null Notes/Work/big.png", &big, &backup::Precond::Any).unwrap();
    assert_eq!(d.download_encrypted("Null Notes/Work/big.png").unwrap(), big);
    // a chunk damaged on Drive is never accepted
    {
        let mut g = m.s.lock().unwrap();
        let p = keys().encrypt_path("Null Notes/Work/n.md").unwrap();
        let h = g.files[&p].chunks[0].0.clone();
        let mut c = g.chunks[&h].clone();
        *c.last_mut().unwrap() ^= 1;
        let nh = backup::sha256_hex(&c);
        g.chunks.insert(nh.clone(), c);
        g.files.get_mut(&p).unwrap().chunks[0].0 = nh;
    }
    assert!(matches!(d.download_encrypted("Null Notes/Work/n.md"), Err(backup::DErr::File(_))));
    assert!(m.s.lock().unwrap().refusals.iter().all(|r| r.contains("e2ee_client_required")), "only the plain client was refused: {:?}", m.s.lock().unwrap().refusals);
}

#[test]
fn e2ee_backup_mode_uploads_only_changes() {
    let m = e2ee_mock::start("tok");
    let root = tmp("bk");
    let v = Vault::open(root.clone()).unwrap();
    v.write_text("Plan.md", "one").unwrap();
    v.write_text("Old.md", "old").unwrap();
    fs::create_dir_all(root.join("Empty folder")).unwrap();
    let mut d = Drive::new(&m.url, "tok").unwrap();
    d.backoff = vec![];
    d.keys = Some(keys());
    let mut st = backup::State::default();
    let r = backup::run_once(&root, FOLDER, &d, &mut st).unwrap();
    assert!(r.problems.is_empty(), "{r:?}");
    assert!(r.uploaded.contains(&"Plan.md".to_string()));
    let r = backup::run_once(&root, FOLDER, &d, &mut st).unwrap();
    assert!(r.uploaded.is_empty(), "unchanged files are compared by keyed hash: {r:?}");
    // a fresh record (another computer, or after a reset): still nothing to send
    let r = backup::run_once(&root, FOLDER, &d, &mut backup::State::default()).unwrap();
    assert!(r.uploaded.is_empty(), "{r:?}");
    later();
    v.write_text("Plan.md", "two").unwrap();
    fs::remove_file(root.join("Old.md")).unwrap();
    let r = backup::run_once(&root, FOLDER, &d, &mut st).unwrap();
    assert_eq!(r.uploaded, vec!["Plan.md".to_string()]);
    assert_eq!(r.deleted, vec!["Old.md".to_string()]);
    assert_eq!(String::from_utf8(d.download_encrypted("Null Notes/Work/Plan.md").unwrap()).unwrap().ends_with("two"), true);
    let view: Vec<String> = drive_view(&m).into_iter().map(|x| x.0).collect();
    assert!(view.contains(&"Null Notes/Work/Plan.md".to_string()) && !view.contains(&"Null Notes/Work/Old.md".to_string()));
    let dirs: Vec<String> = m.s.lock().unwrap().dirs.iter().map(|p| keys().decrypt_path(p).unwrap()).collect();
    assert!(dirs.contains(&"Null Notes/Work/Empty folder".to_string()), "empty folders, encrypted too: {dirs:?}");
    assert_no_plaintext(&m, &["Plan", "Old.md", "Empty folder", "Null Notes"]);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn e2ee_unlock_stores_key_and_pins_fingerprint() {
    let m = e2ee_mock::start("tok");
    let cfg = tmp("cfg");
    let data = tmp("data");
    let bk = backup::Backup::new(&cfg, &data, Box::new(|_| {}));
    let server = m.url.clone();

    // not unlocked yet: runs stop and ask
    assert_eq!(backup::open_drive(&cfg, &server, "tok").err(), Some(backup::DErr::E2ee));
    let info = bk.e2ee_info(&server, Some("tok")).unwrap();
    assert_eq!((info["state"].as_str(), info["unlocked"].as_bool()), (Some("on"), Some(false)));
    assert!(bk.test(&server, Some("tok")).unwrap().contains("unlock it below"));

    // the Drive login password, or a wrong recovery key, are refused with a clear message
    let e = bk.unlock(&server, Some("tok"), "my drive login").unwrap_err();
    assert!(e.contains("not your Drive login password"), "{e}");
    let e = bk.unlock(&server, Some("tok"), "ZZZZ-ZZZZ-ZZZZ-ZZZZ-ZZZZ-ZZZZ-ZZZZ-ZZZZ").unwrap_err();
    assert!(e.contains("recovery key"), "{e}");
    assert!(backup::secrets::get_key(&cfg, &server).is_none(), "nothing stored after a failure");

    // the recovery key works, and so does the passphrase
    bk.unlock(&server, Some("tok"), RECOVERY).unwrap();
    let ok = bk.unlock(&server, Some("tok"), PASSPHRASE).unwrap();
    assert!(ok.contains("5280d8c8b52fe7f7"), "{ok}");
    let stored = backup::secrets::get_key(&cfg, &server).unwrap();
    assert_eq!(stored.fingerprint, "5280d8c8b52fe7f7");
    let secrets_file = fs::read_to_string(cfg.join("secrets.json")).unwrap_or_default();
    assert!(!secrets_file.contains(PASSPHRASE), "the passphrase is never stored");
    let d = backup::open_drive(&cfg, &server, "tok").unwrap();
    assert!(d.encrypted());
    assert!(bk.e2ee_info(&server, Some("tok")).unwrap()["unlocked"].as_bool().unwrap());
    assert!(bk.test(&server, Some("tok")).unwrap().contains("unlocked on this computer"));

    // Drive now shows another fingerprint: stop, don't use the old key
    m.s.lock().unwrap().fingerprint = "00000000deadbeef".into();
    assert_eq!(backup::open_drive(&cfg, &server, "tok").err(), Some(backup::DErr::E2eeKeyChanged));
    assert!(bk.e2ee_info(&server, Some("tok")).unwrap()["key_changed"].as_bool().unwrap());

    bk.forget_key(&server).unwrap();
    assert!(backup::secrets::get_key(&cfg, &server).is_none());
    drop(bk);
    let _ = fs::remove_dir_all(&cfg);
    let _ = fs::remove_dir_all(&data);
}

#[test]
fn e2ee_fresh_device_matches_notes_by_id_without_synced_id() {
    // Drive can't read frontmatter on an encrypted account, so a device with
    // no sync history must learn note ids from the files themselves.
    let m = e2ee_mock::start("tok");
    let mut a = Dev::new("a", &m.url);
    a.v().write_text("Ideas.md", "v1").unwrap();
    a.sync();
    // C: an old copy of the vault (restored from a backup), before the rename
    let c_root = tmp("c");
    fs::copy(a.root.join("Ideas.md"), c_root.join("Ideas.md")).unwrap();
    // meanwhile A renames and edits it
    later();
    fs::create_dir_all(a.root.join("Archive")).unwrap();
    fs::rename(a.root.join("Ideas.md"), a.root.join("Archive/Ideas.md")).unwrap();
    a.v().write_text("Archive/Ideas.md", "v2").unwrap();
    a.sync();
    let mut c = Dev { root: c_root, st: SyncState::default(), drive: { let mut d = Drive::new(&m.url, "tok").unwrap(); d.backoff = vec![]; d.keys = Some(keys()); d } };
    let r = c.sync();
    // matched by note id even without synced_id. With no sync history there's
    // no telling which copy is newer, so both are kept: exactly one copy.
    assert_eq!(r.conflicts.len(), 1, "{r:?}");
    let files = c.files();
    assert_eq!(files.len(), 2, "{files:?}");
    let texts: Vec<String> = files.iter().map(|p| c.read(p)).collect();
    assert!(texts.contains(&"v1".into()) && texts.contains(&"v2".into()), "{texts:?}");
    assert_eq!(drive_view(&m).iter().filter(|(p, _)| p.ends_with(".md")).count(), 2, "Drive has the same two, no duplicates");
    let again = c.sync();
    assert!(again.uploaded.is_empty() && again.downloaded.is_empty() && again.conflicts.is_empty(), "settled in one sync: {again:?}");
    let ra = a.sync();
    assert!(ra.conflicts.is_empty(), "{ra:?}");
    assert_eq!(a.files(), c.files(), "both devices end the same");
    assert!(!c.st.note_ids.is_empty(), "the file id → note id map is kept");
}

#[test]
fn plain_fresh_device_same_scenario_keeps_one_copy() {
    // Phase 7 regression: a Drive move gives the file a new version, and the
    // upload in the same run used to expect the old one -> a second copy.
    let d = crate::sync::mem::Mem::default();
    let ra = tmp("pa");
    let mut sa = SyncState::default();
    Vault::open(ra.clone()).unwrap().write_text("Ideas.md", "v1").unwrap();
    run_sync(&ra, &d, &mut sa, false).unwrap();
    let rc = tmp("pc");
    fs::copy(ra.join("Ideas.md"), rc.join("Ideas.md")).unwrap();
    later();
    fs::create_dir_all(ra.join("Archive")).unwrap();
    fs::rename(ra.join("Ideas.md"), ra.join("Archive/Ideas.md")).unwrap();
    Vault::open(ra.clone()).unwrap().write_text("Archive/Ideas.md", "v2").unwrap();
    while run_sync(&ra, &d, &mut sa, false).unwrap().retry {}
    let mut sc = SyncState::default();
    let r = run_sync(&rc, &d, &mut sc, false).unwrap();
    assert_eq!(r.conflicts.len(), 1, "{r:?}");
    assert!(!r.retry, "the upload after the move used the move's new version: {r:?}");
    let r2 = run_sync(&rc, &d, &mut sc, false).unwrap();
    assert!(r2.conflicts.is_empty() && r2.uploaded.is_empty(), "{r2:?}");
    assert_eq!(d.files.lock().unwrap().len(), 2);
    let _ = fs::remove_dir_all(&ra);
    let _ = fs::remove_dir_all(&rc);
}
