//! Local search agent (search contract v1, Null Notes side).
//!
//! - Binds 127.0.0.1:47821 only.
//! - Every /search request needs `X-Null-Token` (the pairing token, generated
//!   on first run and shown in Settings).
//! - Browsers: only the configured Null Search origin gets CORS headers;
//!   preflights get `Access-Control-Allow-Private-Network: true` when asked
//!   (Chrome's Private Network Access). Other origins get 403.
//! - The Host header must be 127.0.0.1/localhost:47821, which blocks DNS
//!   rebinding (a public site re-pointing its own name at 127.0.0.1).
//! - Searches whatever vault is open; nothing leaves this computer.

use crate::index::Index;
use chacha20poly1305::aead::{rand_core::RngCore, OsRng};
use notify_debouncer_mini::{new_debouncer, notify::RecursiveMode, DebounceEventResult, Debouncer};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tiny_http::{Header, Method, Request, Response, Server};

pub const HOST: &str = "127.0.0.1";
pub const PORT: u16 = 47821;
const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 100;

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct AgentCfg {
    pub token: String,
    /// The one browser origin allowed to call the agent, e.g. "http://search.lan".
    #[serde(default)]
    pub origin: String,
}

#[derive(Serialize)]
pub struct AgentInfo {
    pub token: String,
    pub origin: String,
    pub address: String,
    pub status: String,
    pub indexed: bool,
}

pub struct Search {
    index: Mutex<Option<Index>>,
    watcher: Mutex<Option<Debouncer<notify_debouncer_mini::notify::RecommendedWatcher>>>,
    cfg: Mutex<AgentCfg>,
    cfg_path: PathBuf,
    db_dir: PathBuf,
    status: Mutex<String>,
}

fn new_token() -> String {
    let mut b = [0u8; 32];
    OsRng.fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Stable across builds (unlike std's hasher), so a vault keeps its index file.
fn fnv(s: &str) -> u64 {
    s.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3))
}

fn ct_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// "HTTP://Search.LAN:8080/" -> "http://search.lan:8080". Empty is allowed (no browser access).
pub fn clean_origin(o: &str) -> Result<String, String> {
    let o = o.trim().trim_end_matches('/');
    if o.is_empty() {
        return Ok(String::new());
    }
    let lower = o.to_ascii_lowercase();
    let rest = lower.strip_prefix("http://").or_else(|| lower.strip_prefix("https://"));
    match rest {
        Some(h) if !h.is_empty() && !h.contains(['/', '?', '#', ' ', '@']) => Ok(lower),
        _ => Err("Enter the Null Search address like http://search.lan or https://search.example.com (no path).".into()),
    }
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                match u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn query_param(url: &str, key: &str) -> Option<String> {
    let q = url.split_once('?')?.1;
    q.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        (pct_decode(k) == key).then(|| pct_decode(v))
    })
}

fn header<'a>(req: &'a Request, name: &str) -> Option<&'a str> {
    req.headers().iter().find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name)).map(|h| h.value.as_str())
}

fn h(k: &str, v: &str) -> Header {
    Header::from_bytes(k.as_bytes(), v.as_bytes()).unwrap()
}

impl Search {
    pub fn new(config_dir: &Path, data_dir: &Path) -> Arc<Search> {
        let cfg_path = config_dir.join("agent.json");
        let mut cfg: AgentCfg = std::fs::read(&cfg_path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let s = Arc::new(Search {
            index: Mutex::new(None),
            watcher: Mutex::new(None),
            cfg: Mutex::new(AgentCfg::default()),
            cfg_path,
            db_dir: data_dir.join("index"),
            status: Mutex::new("Starting…".into()),
        });
        if cfg.token.len() < 32 {
            // First run: mint the pairing token.
            cfg.token = new_token();
            *s.cfg.lock().unwrap() = cfg;
            s.save_cfg();
        } else {
            *s.cfg.lock().unwrap() = cfg;
        }
        s
    }

    fn save_cfg(&self) {
        let c = self.cfg.lock().unwrap().clone();
        if let Some(d) = self.cfg_path.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = crate::vault::atomic_write(&self.cfg_path, serde_json::to_string_pretty(&c).unwrap().as_bytes());
    }

    pub fn info(&self) -> AgentInfo {
        let c = self.cfg.lock().unwrap().clone();
        AgentInfo {
            token: c.token,
            origin: c.origin,
            address: format!("http://{HOST}:{PORT}"),
            status: self.status.lock().unwrap().clone(),
            indexed: self.index.lock().unwrap().is_some(),
        }
    }

    pub fn set_origin(&self, origin: &str) -> Result<AgentInfo, String> {
        let o = clean_origin(origin)?;
        self.cfg.lock().unwrap().origin = o;
        self.save_cfg();
        Ok(self.info())
    }

    pub fn regenerate_token(&self) -> AgentInfo {
        self.cfg.lock().unwrap().token = new_token();
        self.save_cfg();
        self.info()
    }

    /// Point the index and watcher at a vault (or at nothing when it closes).
    pub fn set_vault(self: &Arc<Self>, root: Option<PathBuf>) {
        *self.watcher.lock().unwrap() = None;
        *self.index.lock().unwrap() = None;
        let Some(root) = root else { return };
        let db = self.db_dir.join(format!("{:016x}.db", fnv(&root.to_string_lossy())));
        match Index::open(&db, root.clone()) {
            Ok(ix) => *self.index.lock().unwrap() = Some(ix),
            Err(e) => {
                *self.status.lock().unwrap() = format!("Couldn't open the search index: {e}");
                return;
            }
        }
        // First full pass in the background (it may backfill ids on an old vault).
        let me = self.clone();
        std::thread::spawn(move || me.resync());
        // Then follow changes made by this app, Explorer, git, sync tools…
        let me = Arc::downgrade(self);
        let deb = new_debouncer(Duration::from_millis(600), move |res: DebounceEventResult| {
            if let (Ok(events), Some(me)) = (res, me.upgrade()) {
                let relevant = events.iter().any(|e| {
                    let p = e.path.to_string_lossy();
                    !p.contains(".nullnotes") && !p.ends_with(".nntmp")
                });
                if relevant {
                    me.resync();
                }
            }
        });
        match deb {
            Ok(mut d) => {
                if d.watcher().watch(&root, RecursiveMode::Recursive).is_ok() {
                    *self.watcher.lock().unwrap() = Some(d);
                }
            }
            Err(e) => *self.status.lock().unwrap() = format!("File watcher unavailable: {e}"),
        }
    }

    pub fn resync(&self) {
        if let Some(ix) = self.index.lock().unwrap().as_mut() {
            let _ = ix.sync();
        }
    }

    pub fn renamed(&self, from: &str, to: &str) {
        if let Some(ix) = self.index.lock().unwrap().as_ref() {
            let _ = ix.rename(from, to);
        }
    }

    /// Start the HTTP agent on its own thread.
    pub fn serve(self: &Arc<Self>) {
        let server = match Server::http((HOST, PORT)) {
            Ok(s) => s,
            Err(e) => {
                *self.status.lock().unwrap() = format!("Not running: port {PORT} is in use or blocked ({e}). Is another copy of Null Notes open?");
                return;
            }
        };
        *self.status.lock().unwrap() = format!("Listening on http://{HOST}:{PORT}");
        let me = self.clone();
        std::thread::spawn(move || {
            for req in server.incoming_requests() {
                me.handle(req);
            }
        });
    }

    fn handle(&self, req: Request) {
        let json = |code: u16, body: String| Response::from_string(body).with_status_code(code).with_header(h("Content-Type", "application/json")).with_header(h("Cache-Control", "no-store"));
        let err = |code: u16, msg: &str| json(code, serde_json::json!({ "error": msg }).to_string());

        // DNS-rebinding guard
        let host_ok = header(&req, "Host").map(|v| {
            let v = v.to_ascii_lowercase();
            v == format!("{HOST}:{PORT}") || v == format!("localhost:{PORT}")
        });
        if host_ok != Some(true) {
            let _ = req.respond(err(403, "bad Host header"));
            return;
        }

        let cfg = self.cfg.lock().unwrap().clone();
        let origin = header(&req, "Origin").map(|s| s.to_string());
        let cors_ok = match &origin {
            None => true, // not a browser (curl, Null Search's server side)
            Some(o) => !cfg.origin.is_empty() && o.trim_end_matches('/').eq_ignore_ascii_case(&cfg.origin),
        };
        if !cors_ok {
            let _ = req.respond(err(403, "origin not allowed"));
            return;
        }
        let add_cors = |mut r: Response<std::io::Cursor<Vec<u8>>>| {
            if let Some(o) = &origin {
                r.add_header(h("Access-Control-Allow-Origin", o));
                r.add_header(h("Vary", "Origin"));
            }
            r
        };

        let path = req.url().split('?').next().unwrap_or("").to_string();
        if path != "/search" {
            let _ = req.respond(add_cors(err(404, "not found")));
            return;
        }

        match req.method() {
            Method::Options => {
                let mut r = Response::from_data(Vec::new()).with_status_code(204);
                r.add_header(h("Access-Control-Allow-Methods", "GET, OPTIONS"));
                r.add_header(h("Access-Control-Allow-Headers", "X-Null-Token"));
                r.add_header(h("Access-Control-Max-Age", "600"));
                if header(&req, "Access-Control-Request-Private-Network").map(|v| v.eq_ignore_ascii_case("true")).unwrap_or(false) {
                    r.add_header(h("Access-Control-Allow-Private-Network", "true"));
                }
                let _ = req.respond(add_cors(r));
            }
            Method::Get => {
                let tok = header(&req, "X-Null-Token").unwrap_or("");
                if !ct_eq(tok, &cfg.token) {
                    let _ = req.respond(add_cors(err(401, "missing or wrong X-Null-Token")));
                    return;
                }
                let q = query_param(req.url(), "q").unwrap_or_default();
                let limit = query_param(req.url(), "limit").and_then(|l| l.parse::<usize>().ok()).unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
                let hits = match self.index.lock().unwrap().as_ref() {
                    Some(ix) => ix.search(&q, limit).unwrap_or_default(),
                    None => vec![], // no vault open
                };
                let _ = req.respond(add_cors(json(200, serde_json::to_string(&hits).unwrap())));
            }
            _ => {
                let _ = req.respond(add_cors(err(405, "method not allowed")));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn helpers() {
        assert_eq!(pct_decode("libre+ai%21"), "libre ai!");
        assert_eq!(pct_decode("100%"), "100%");
        assert_eq!(query_param("/search?q=libre%20ai&limit=5", "q").as_deref(), Some("libre ai"));
        assert_eq!(clean_origin("HTTP://Search.lan:8080/").unwrap(), "http://search.lan:8080");
        assert!(clean_origin("http://x/y").is_err());
        assert!(clean_origin("ftp://x").is_err());
        assert!(ct_eq("abc", "abc") && !ct_eq("abc", "abd") && !ct_eq("ab", "abc"));
    }
}
