//! Null Notes backend. Command names and argument names match what the
//! frontend (src/backend.js) invokes.

mod agent;
mod backup;
mod index;
mod links;
mod lock;
mod noteid;
mod vault;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_deep_link::DeepLinkExt;
use tauri_plugin_dialog::DialogExt;
use vault::{Entry, ImportReport, Vault, R};

const RECENT_MAX: usize = 10;

#[derive(Serialize)]
struct VaultInfo {
    path: String,
    name: String,
}

#[derive(Serialize)]
struct LockStatus {
    configured: bool,
    unlocked: bool,
}

/// App-level settings in the OS config dir (%APPDATA%\dev.nullgrid.notes\settings.json).
#[derive(Serialize, Deserialize, Default)]
struct Settings {
    last: Option<String>,
    #[serde(default)]
    recent: Vec<String>,
}

#[derive(Default)]
struct AppState {
    vault: Mutex<Option<Vault>>,
    /// A nullnotes:// link waiting for the frontend to open it.
    pending_open: Mutex<Option<links::OpenRequest>>,
}

fn settings_path(app: &AppHandle) -> Option<PathBuf> {
    app.path().app_config_dir().ok().map(|d| d.join("settings.json"))
}

fn load_settings(app: &AppHandle) -> Settings {
    settings_path(app)
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_settings(app: &AppHandle, s: &Settings) {
    if let Some(p) = settings_path(app) {
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = vault::atomic_write(&p, serde_json::to_string_pretty(s).unwrap().as_bytes());
    }
}

fn info(v: &Vault) -> VaultInfo {
    VaultInfo { path: v.root.display().to_string(), name: v.name() }
}

fn with<T>(st: &State<AppState>, f: impl FnOnce(&Vault) -> R<T>) -> R<T> {
    let g = st.vault.lock().unwrap();
    f(g.as_ref().ok_or("No vault is open.")?)
}

fn with_mut<T>(st: &State<AppState>, f: impl FnOnce(&mut Vault) -> R<T>) -> R<T> {
    let mut g = st.vault.lock().unwrap();
    f(g.as_mut().ok_or("No vault is open.")?)
}

fn enter(app: &AppHandle, st: &State<AppState>, path: PathBuf) -> R<VaultInfo> {
    let v = Vault::open(path)?;
    let i = info(&v);
    let mut s = load_settings(app);
    s.recent.retain(|p| !p.eq_ignore_ascii_case(&i.path));
    s.recent.insert(0, i.path.clone());
    s.recent.truncate(RECENT_MAX);
    s.last = Some(i.path.clone());
    save_settings(app, &s);
    let root = v.root.clone();
    let name = v.name();
    *st.vault.lock().unwrap() = Some(v);
    app.state::<Arc<agent::Search>>().set_vault(Some(root.clone()));
    app.state::<Arc<backup::Backup>>().set_vault(Some((root, name)));
    Ok(i)
}

/* ---------------- vaults ---------------- */

#[tauri::command]
fn vault_info(st: State<AppState>) -> Option<VaultInfo> {
    st.vault.lock().unwrap().as_ref().map(info)
}

#[tauri::command]
fn recent_vaults(app: AppHandle) -> Vec<VaultInfo> {
    load_settings(&app)
        .recent
        .into_iter()
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .map(|p| VaultInfo {
            name: p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
            path: p.display().to_string(),
        })
        .collect()
}

#[tauri::command]
fn open_vault(app: AppHandle, st: State<AppState>, path: String) -> R<VaultInfo> {
    enter(&app, &st, PathBuf::from(path))
}

#[tauri::command]
async fn pick_vault(app: AppHandle, st: State<'_, AppState>) -> R<Option<VaultInfo>> {
    let picked = app
        .dialog()
        .file()
        .set_title("Choose a folder for your vault (an empty folder starts a new one)")
        .blocking_pick_folder();
    match picked.and_then(|f| f.into_path().ok()) {
        Some(p) => enter(&app, &st, p).map(Some),
        None => Ok(None),
    }
}

#[tauri::command]
fn close_vault(app: AppHandle, st: State<AppState>) {
    *st.vault.lock().unwrap() = None;
    app.state::<Arc<agent::Search>>().set_vault(None);
    app.state::<Arc<backup::Backup>>().set_vault(None);
    let mut s = load_settings(&app);
    s.last = None;
    save_settings(&app, &s);
}

/* ---------------- files ---------------- */

#[tauri::command]
fn list_entries(st: State<AppState>) -> R<Vec<Entry>> {
    with(&st, |v| v.list_entries())
}

#[tauri::command]
fn read_all_text(st: State<AppState>) -> R<BTreeMap<String, String>> {
    with(&st, |v| v.read_all_text())
}

#[tauri::command]
fn read_text(st: State<AppState>, path: String) -> R<String> {
    with(&st, |v| v.read_text(&path))
}

#[tauri::command]
fn read_image(st: State<AppState>, path: String) -> R<String> {
    with(&st, |v| v.read_image(&path))
}

#[tauri::command]
fn write_text(st: State<AppState>, path: String, content: String) -> R<()> {
    with(&st, |v| v.write_text(&path, &content))
}

#[tauri::command]
fn write_image(st: State<AppState>, path: String, data: String) -> R<()> {
    with(&st, |v| v.write_image(&path, &data))
}

#[tauri::command]
fn create_file(st: State<AppState>, path: String, content: String) -> R<()> {
    with(&st, |v| v.create_file(&path, &content))
}

#[tauri::command]
fn create_folder(st: State<AppState>, path: String) -> R<()> {
    with(&st, |v| v.create_folder(&path))
}

#[tauri::command]
fn rename_path(st: State<AppState>, search: State<Arc<agent::Search>>, bk: State<Arc<backup::Backup>>, from: String, to: String) -> R<()> {
    with(&st, |v| v.rename_path(&from, &to))?;
    if let (Ok(f), Ok(t)) = (vault::clean(&from), vault::clean(&to)) {
        search.renamed(&f, &t);
        bk.renamed(&f, &t);
    }
    Ok(())
}

/* ---------------- back up to Null Drive (Settings) ---------------- */

#[tauri::command]
fn backup_status(bk: State<Arc<backup::Backup>>) -> backup::Status {
    bk.status()
}

/// `token`: None keeps the saved one, "" removes it.
#[tauri::command]
fn backup_configure(bk: State<Arc<backup::Backup>>, enabled: bool, server: String, folder: String, token: Option<String>) -> R<backup::Status> {
    bk.configure(enabled, &server, &folder, token.as_deref())
}

#[tauri::command]
async fn backup_test(bk: State<'_, Arc<backup::Backup>>, server: String, token: Option<String>) -> R<String> {
    bk.test(&server, token.as_deref())
}

#[tauri::command]
fn backup_now(bk: State<Arc<backup::Backup>>) {
    bk.back_up_now()
}

#[tauri::command]
fn delete_path(st: State<AppState>, path: String, permanent: Option<bool>) -> R<()> {
    with(&st, |v| v.delete_path(&path, permanent.unwrap_or(false)))
}

#[tauri::command]
fn import_paths(st: State<AppState>, paths: Vec<String>, dest: String) -> R<ImportReport> {
    with(&st, |v| v.import_paths(&paths, &dest))
}

#[tauri::command]
async fn pick_import(app: AppHandle, st: State<'_, AppState>, dest: String, folders: Option<bool>) -> R<Option<ImportReport>> {
    let d = app.dialog().file();
    let picked: Option<Vec<PathBuf>> = if folders.unwrap_or(false) {
        d.set_title("Import folders into the vault")
            .blocking_pick_folders()
            .map(|v| v.into_iter().filter_map(|f| f.into_path().ok()).collect())
    } else {
        d.set_title("Import files into the vault")
            .add_filter("Notes, pages and images", &["md", "markdown", "txt", "html", "htm", "canvas", "png", "jpg", "jpeg"])
            .blocking_pick_files()
            .map(|v| v.into_iter().filter_map(|f| f.into_path().ok()).collect())
    };
    let Some(paths) = picked.filter(|p| !p.is_empty()) else { return Ok(None) };
    let paths: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
    with(&st, |v| v.import_paths(&paths, &dest)).map(Some)
}

#[tauri::command]
fn reveal_path(st: State<AppState>, path: String) -> R<()> {
    let abs = with(&st, |v| Ok(v.abs(&vault::clean(&path)?)))?;
    #[cfg(windows)]
    let r = if path.is_empty() {
        std::process::Command::new("explorer").arg(&abs).spawn()
    } else {
        let mut arg = std::ffi::OsString::from("/select,");
        arg.push(&abs);
        std::process::Command::new("explorer").arg(arg).spawn()
    };
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").arg("-R").arg(&abs).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let r = std::process::Command::new("xdg-open").arg(if abs.is_dir() { abs.clone() } else { abs.parent().unwrap().to_path_buf() }).spawn();
    r.map(|_| ()).map_err(|e| format!("Couldn't open the file manager: {e}"))
}

/* ---------------- search agent (Settings → Null Search) ---------------- */

#[tauri::command]
fn agent_info(search: State<Arc<agent::Search>>) -> agent::AgentInfo {
    search.info()
}

#[tauri::command]
fn agent_set_origin(search: State<Arc<agent::Search>>, origin: String) -> R<agent::AgentInfo> {
    search.set_origin(&origin)
}

#[tauri::command]
fn agent_new_token(search: State<Arc<agent::Search>>) -> agent::AgentInfo {
    search.regenerate_token()
}

/* ---------------- per-vault meta ---------------- */

#[tauri::command]
fn read_meta(st: State<AppState>, name: String) -> R<Option<String>> {
    with(&st, |v| v.read_meta(&name))
}

#[tauri::command]
fn write_meta(st: State<AppState>, name: String, content: String) -> R<()> {
    with(&st, |v| v.write_meta(&name, &content))
}

/* ---------------- lock ---------------- */

#[tauri::command]
fn lock_status(st: State<AppState>) -> R<LockStatus> {
    with(&st, |v| Ok(LockStatus { configured: v.lock_configured(), unlocked: v.key.is_some() }))
}

// Argon2id with 64 MiB takes a moment, so these run off the main thread.
#[tauri::command]
async fn lock_setup(st: State<'_, AppState>, passphrase: String) -> R<()> {
    with_mut(&st, |v| v.lock_setup(&passphrase))
}

#[tauri::command]
async fn lock_unlock(st: State<'_, AppState>, passphrase: String) -> R<()> {
    with_mut(&st, |v| v.lock_unlock(&passphrase))
}

#[tauri::command]
async fn lock_change(st: State<'_, AppState>, old: String, r#new: String) -> R<()> {
    with_mut(&st, |v| v.lock_change(&old, &r#new))
}

#[tauri::command]
fn lock_lock(st: State<AppState>) -> R<()> {
    with_mut(&st, |v| {
        v.key = None;
        Ok(())
    })
}

#[tauri::command]
fn encrypt_path(st: State<AppState>, path: String) -> R<String> {
    with(&st, |v| v.encrypt_path(&path))
}

#[tauri::command]
fn decrypt_path(st: State<AppState>, path: String) -> R<String> {
    with(&st, |v| v.decrypt_path(&path))
}

#[tauri::command]
fn read_locked(st: State<AppState>, path: String) -> R<String> {
    with(&st, |v| v.read_locked(&path))
}

#[tauri::command]
fn write_locked(st: State<AppState>, path: String, content: String) -> R<()> {
    with(&st, |v| v.write_locked(&path, &content))
}

#[tauri::command]
fn create_locked(st: State<AppState>, path: String, content: String) -> R<()> {
    with(&st, |v| v.create_locked(&path, &content))
}

#[tauri::command]
fn read_all_locked(st: State<AppState>) -> R<BTreeMap<String, String>> {
    with(&st, |v| v.read_all_locked())
}

/* ---------------- nullnotes:// links ---------------- */

fn focus_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

/// Resolve links off the main thread (a cold start may still be indexing),
/// then hand the result to the frontend.
fn handle_links(app: &AppHandle, urls: Vec<String>) {
    let Some(link) = urls.into_iter().find(|u| u.to_ascii_lowercase().starts_with("nullnotes:")) else { return };
    let app = app.clone();
    std::thread::spawn(move || {
        let st = app.state::<AppState>();
        let open_root = st.vault.lock().unwrap().as_ref().map(|v| v.root.clone());
        let search = app.state::<Arc<agent::Search>>();
        let recent: Vec<PathBuf> = load_settings(&app).recent.into_iter().map(PathBuf::from).collect();
        let req = links::resolve(&link, open_root, |id| search.find(id), &recent);
        *st.pending_open.lock().unwrap() = Some(req);
        let _ = app.emit("open-note", ());
        focus_main(&app);
    });
}

/// The frontend calls this on start-up and on every "open-note" event.
#[tauri::command]
fn take_pending_open(st: State<AppState>) -> Option<links::OpenRequest> {
    st.pending_open.lock().unwrap().take()
}

pub fn build(b: tauri::Builder<tauri::Wry>) -> tauri::Builder<tauri::Wry> {
    // single-instance must be the first plugin: a second launch (e.g. Windows
    // starting us for a link) forwards its arguments here and exits.
    b.plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| focus_main(app)))
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::default())
        .setup(|app| {
            let h = app.handle().clone();
            let cfg_dir = h.path().app_config_dir()?;
            let data_dir = h.path().app_local_data_dir()?;
            let search = agent::Search::new(&cfg_dir, &data_dir);
            search.serve();
            app.manage(search.clone());
            let eh = h.clone();
            let bk = backup::Backup::new(&cfg_dir, &data_dir, Box::new(move |s: &backup::Status| {
                let _ = eh.emit("backup-status", s.clone());
            }));
            let kick = Arc::downgrade(&bk);
            search.set_on_change(Box::new(move || {
                if let Some(b) = kick.upgrade() {
                    b.kick();
                }
            }));
            app.manage(bk.clone());
            // Reopen the last vault, like the shipped app.
            if let Some(last) = load_settings(&h).last {
                if let Ok(v) = Vault::open(PathBuf::from(last)) {
                    search.set_vault(Some(v.root.clone()));
                    bk.set_vault(Some((v.root.clone(), v.name())));
                    *app.state::<AppState>().vault.lock().unwrap() = Some(v);
                }
            }
            // Installers (NSIS/MSI) and the MSIX manifest register nullnotes://.
            // Dev builds and Linux register the running binary instead.
            #[cfg(any(target_os = "linux", all(debug_assertions, windows)))]
            if let Err(e) = app.deep_link().register_all() {
                eprintln!("couldn't register nullnotes:// for this build: {e}");
            }
            let lh = h.clone();
            app.deep_link().on_open_url(move |ev| handle_links(&lh, ev.urls().iter().map(|u| u.to_string()).collect()));
            // Cold start: Windows launched us with the link as an argument.
            if let Ok(Some(urls)) = app.deep_link().get_current() {
                handle_links(&h, urls.iter().map(|u| u.to_string()).collect());
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            vault_info, recent_vaults, open_vault, pick_vault, close_vault,
            list_entries, read_all_text, read_text, read_image, write_text, write_image,
            create_file, create_folder, rename_path, delete_path, import_paths, pick_import, reveal_path,
            read_meta, write_meta,
            lock_status, lock_setup, lock_unlock, lock_change, lock_lock,
            encrypt_path, decrypt_path, read_locked, write_locked, create_locked, read_all_locked,
            agent_info, agent_set_origin, agent_new_token,
            take_pending_open,
            backup_status, backup_configure, backup_test, backup_now,
        ])
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    build(tauri::Builder::default())
        .run(tauri::generate_context!())
        .expect("error while running Null Notes");
}
