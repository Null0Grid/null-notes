//! In-process stand-in for an end-to-end encrypted Null Drive account
//! (server 0.8.3+), for tests. Like the real server it never has the key:
//! it only enforces what notes-e2ee-drive-api.md says the server enforces.
//!
//! - `X-NullDrive-E2EE: 1` required on listings and v2 calls (409 e2ee_client_required)
//! - v1 whole-file PUT/GET refused (409 e2ee_required)
//! - every path segment must be `~…` (409 e2ee_path_required)
//! - chunks verified by SHA-256 on upload (400 hash_mismatch); a commit must
//!   reference stored chunks (422 missing_chunks) whose sizes minus 41 bytes
//!   each add up to `size` (400 invalid_manifest)
//! - versions, `If-Match` / `If-None-Match: *`, 409 conflict + current, 428,
//!   identical content (same `sha256`) is a 200 no-op, moves keep the id
//! - no `synced_id`, ever
//!
//! Everything received is logged, so tests can check that no plaintext name
//! or content ever reached "the server".

use std::collections::{BTreeMap, HashSet};
#[allow(unused_imports)]
use std::io::Read;
use std::sync::{Arc, Mutex};
use tiny_http::{Header, Response, Server};

pub const BUNDLE: &str = include_str!("../testdata/e2ee/vectors.json");
pub const PASSPHRASE: &str = "vector passphrase 123";
pub const RECOVERY: &str = "B9D5-MPJT-B9D5-MPJT-B9D5-MPJT-B9D5-MPJT";

#[derive(Clone, Debug)]
pub struct F {
    pub id: String,
    pub version: i64,
    pub size: u64,
    pub sha256: String,
    pub chunks: Vec<(String, u64)>,
}

#[derive(Default)]
pub struct S {
    pub files: BTreeMap<String, F>,
    pub dirs: HashSet<String>,
    pub chunks: BTreeMap<String, Vec<u8>>,
    pub trash: Vec<String>,
    pub counter: i64,
    /// "METHOD path" + body, verbatim, of every request
    pub log: Vec<String>,
    /// every byte received (URLs, headers' values, bodies)
    pub wire: Vec<u8>,
    pub fingerprint: String,
    /// rows listed under the prefix that don't decrypt (a leftover / other keys)
    pub junk_rows: Vec<serde_json::Value>,
    pub hook: Option<Box<dyn FnMut(&mut S) + Send>>,
    pub refusals: Vec<String>,
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

fn encrypted(p: &str) -> bool {
    !p.is_empty() && p.split('/').all(|s| s.starts_with('~') && s.len() >= 76)
}

fn row(p: &str, f: &F) -> serde_json::Value {
    serde_json::json!({"id": f.id, "path": p, "is_dir": false, "size": f.size, "sha256": f.sha256, "version": f.version, "deleted": false})
}

fn sha(b: &[u8]) -> String {
    crate::backup::sha256_hex(b)
}

pub fn start(token: &str) -> Mock {
    let server = Server::http("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", server.server_addr().to_ip().unwrap().port());
    let v: serde_json::Value = serde_json::from_str(BUNDLE).unwrap();
    let s = Arc::new(Mutex::new(S { fingerprint: v["fingerprint"].as_str().unwrap().into(), ..Default::default() }));
    let st = s.clone();
    let token = token.to_string();
    let bundle = v["bundle"].clone();
    std::thread::spawn(move || {
        for mut req in server.incoming_requests() {
            let j = |code: u16, v: serde_json::Value| Response::from_string(v.to_string()).with_status_code(code).with_header(Header::from_bytes("Content-Type", "application/json").unwrap());
            let err = |code: u16, c: &str, cur: Option<serde_json::Value>| j(code, serde_json::json!({"error": {"code": c, "message": c, "current": cur}}));
            let hdr = |r: &tiny_http::Request, n: &str| r.headers().iter().find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(n)).map(|h| h.value.to_string());
            let method = req.method().to_string();
            let url = req.url().to_string();
            let (path, query) = url.split_once('?').map(|(a, b)| (a.to_string(), b.to_string())).unwrap_or((url.clone(), String::new()));
            let mut body = Vec::new();
            let _ = req.as_reader().read_to_end(&mut body);
            let mut g = st.lock().unwrap();
            g.wire.extend(dec(&url).as_bytes());
            for h in req.headers() {
                g.wire.extend(h.value.as_str().as_bytes());
            }
            g.wire.extend(&body);
            g.log.push(format!("{method} {}", dec(&path)));
            if hdr(&req, "Authorization") != Some(format!("Bearer {token}")) {
                let _ = req.respond(err(401, "unauthorized", None));
                continue;
            }
            let e2ee_hdr = hdr(&req, "X-NullDrive-E2EE").as_deref() == Some("1");
            let if_match = hdr(&req, "If-Match").map(|v| v.trim_matches('"').to_string());
            let if_none = hdr(&req, "If-None-Match");
            if method != "GET" {
                if let Some(mut h) = g.hook.take() {
                    h(&mut g);
                }
            }
            let refuse = |g: &mut S, code: u16, c: &str| {
                g.refusals.push(format!("{code} {c} {method} {}", dec(&path)));
                err(code, c, None)
            };
            let needs_hdr = path == "/api/v1/files" || (path.starts_with("/api/v2/") && path != "/api/v2/e2ee");
            if needs_hdr && !e2ee_hdr {
                let r = refuse(&mut g, 409, "e2ee_client_required");
                let _ = req.respond(r);
                continue;
            }
            let resp = match (method.as_str(), path.as_str()) {
                ("GET", "/api/v1/me") => j(200, serde_json::json!({"user": {"name": "luke", "e2ee": "on"}})),
                ("GET", "/api/v2/e2ee") => j(200, serde_json::json!({"state": "on", "fingerprint": g.fingerprint, "enabled_at": "2026-10-18T07:12:00Z", "plaintext_files": 0, "bundle": bundle})),
                ("PUT", p) | ("GET", p) if p.starts_with("/api/v1/files/") => refuse(&mut g, 409, "e2ee_required"),
                ("POST", "/api/v1/folders") => {
                    let b: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
                    let p = b["path"].as_str().unwrap_or("").to_string();
                    if !encrypted(&p) {
                        refuse(&mut g, 409, "e2ee_path_required")
                    } else {
                        let segs: Vec<&str> = p.split('/').collect();
                        for i in 1..=segs.len() {
                            g.dirs.insert(segs[..i].join("/"));
                        }
                        j(201, serde_json::json!({"path": p}))
                    }
                }
                ("GET", "/api/v1/files") => {
                    let prefix = dec(query.split('&').find_map(|kv| kv.strip_prefix("prefix=")).unwrap_or(""));
                    if !prefix.is_empty() && !encrypted(&prefix) {
                        refuse(&mut g, 409, "e2ee_path_required")
                    } else {
                        let pre = format!("{prefix}/");
                        let mut rows: Vec<serde_json::Value> = g.files.iter().filter(|(p, _)| p.starts_with(&pre)).map(|(p, f)| row(p, f)).collect();
                        rows.extend(g.dirs.iter().filter(|d| d.starts_with(&pre)).map(|d| serde_json::json!({"id": "dir", "path": d, "is_dir": true, "size": 0, "sha256": "", "version": 0})));
                        rows.extend(g.junk_rows.iter().map(|r| {
                            let mut r = r.clone();
                            r["path"] = serde_json::Value::String(format!("{prefix}/{}", r["path"].as_str().unwrap()));
                            r
                        }));
                        let v = g.counter;
                        j(200, serde_json::json!({"version": v, "files": rows}))
                    }
                }
                ("POST", "/api/v2/chunks/check") => {
                    let b: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
                    let missing: Vec<String> = b["hashes"].as_array().map(|a| a.iter().filter_map(|h| h.as_str()).filter(|h| !g.chunks.contains_key(*h)).map(String::from).collect()).unwrap_or_default();
                    j(200, serde_json::json!({"missing": missing}))
                }
                ("PUT", p) if p.starts_with("/api/v2/chunks/") => {
                    let h = &p["/api/v2/chunks/".len()..];
                    if sha(&body) != h {
                        refuse(&mut g, 400, "hash_mismatch")
                    } else if body.len() > 16 * 1024 * 1024 || body.first() != Some(&1) {
                        refuse(&mut g, 400, "invalid_chunk")
                    } else {
                        let n = body.len();
                        g.chunks.insert(h.to_string(), body.clone());
                        j(201, serde_json::json!({"hash": h, "size": n}))
                    }
                }
                ("GET", p) if p.starts_with("/api/v2/chunks/") => match g.chunks.get(&p["/api/v2/chunks/".len()..]) {
                    Some(b) => Response::from_data(b.clone()).with_status_code(200),
                    None => err(404, "not_found", None),
                },
                ("GET", p) if p.starts_with("/api/v2/files/") && p.ends_with("/manifest") => {
                    let rp = dec(&p["/api/v2/files/".len()..p.len() - "/manifest".len()]);
                    match g.files.get(&rp) {
                        Some(f) => j(200, serde_json::json!({"path": rp, "id": f.id, "version": f.version, "size": f.size, "sha256": f.sha256,
                                                             "chunks": f.chunks.iter().map(|(h, n)| serde_json::json!({"hash": h, "size": n})).collect::<Vec<_>>()}))
                            .with_header(Header::from_bytes("ETag", format!("\"{}\"", f.version)).unwrap()),
                        None => err(404, "not_found", None),
                    }
                }
                ("PUT", p) if p.starts_with("/api/v2/files/") => {
                    let rp = dec(&p["/api/v2/files/".len()..]);
                    let b: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
                    if !encrypted(&rp) {
                        refuse(&mut g, 409, "e2ee_path_required")
                    } else {
                        let size = b["size"].as_u64().unwrap_or(u64::MAX);
                        let shaf = b["sha256"].as_str().unwrap_or("").to_string();
                        let chunks: Vec<(String, u64)> = b["chunks"].as_array().map(|a| a.iter().map(|c| (c["hash"].as_str().unwrap_or("").to_string(), c["size"].as_u64().unwrap_or(0))).collect()).unwrap_or_default();
                        let missing: Vec<&String> = chunks.iter().map(|(h, _)| h).filter(|h| !g.chunks.contains_key(*h)).collect();
                        let sum: i64 = chunks.iter().map(|(_, n)| *n as i64 - 41).sum();
                        let sizes_ok = chunks.iter().all(|(h, n)| g.chunks.get(h).map(|c| c.len() as u64 == *n).unwrap_or(true));
                        if shaf.len() != 64 || sum != size as i64 || !sizes_ok {
                            refuse(&mut g, 400, "invalid_manifest")
                        } else if !missing.is_empty() {
                            let m: Vec<String> = missing.into_iter().cloned().collect();
                            j(422, serde_json::json!({"error": {"code": "missing_chunks", "message": "missing_chunks"}, "missing": m}))
                        } else {
                            let cur = g.files.get(&rp).cloned();
                            let same = cur.as_ref().map(|f| f.sha256 == shaf && f.size == size).unwrap_or(false);
                            let curj = cur.as_ref().map(|f| row(&rp, f));
                            let ok = match (&if_none, &if_match, &cur) {
                                (Some(_), None, c) => c.is_none() || same,
                                (None, Some(m), _) if m == "*" => true,
                                (None, Some(m), Some(f)) => f.version.to_string() == *m || same,
                                (None, Some(_), None) => false,
                                (None, None, Some(_)) => {
                                    let r = refuse(&mut g, 428, "precondition_required");
                                    let _ = req.respond(r);
                                    continue;
                                }
                                _ => true,
                            };
                            if !ok {
                                err(409, "conflict", curj).with_header(Header::from_bytes("ETag", format!("\"{}\"", cur.as_ref().map(|f| f.version).unwrap_or(0))).unwrap())
                            } else if same {
                                j(200, curj.unwrap())
                            } else {
                                let v = g.bump();
                                let id = cur.map(|f| f.id).unwrap_or_else(|| format!("{v:032x}"));
                                let f = F { id, version: v, size, sha256: shaf, chunks };
                                let r = row(&rp, &f);
                                let segs: Vec<String> = rp.split('/').map(String::from).collect();
                                for i in 1..segs.len() {
                                    let d = segs[..i].join("/");
                                    g.dirs.insert(d);
                                }
                                g.files.insert(rp, f);
                                j(201, r).with_header(Header::from_bytes("ETag", format!("\"{v}\"")).unwrap())
                            }
                        }
                    }
                }
                ("DELETE", p) if p.starts_with("/api/v1/files/") => {
                    let rp = dec(&p["/api/v1/files/".len()..]);
                    if !encrypted(&rp) {
                        refuse(&mut g, 409, "e2ee_path_required")
                    } else {
                        match g.files.get(&rp).cloned() {
                            None if g.dirs.contains(&rp) => {
                                let pre = format!("{rp}/");
                                let gone: Vec<String> = g.files.keys().filter(|k| k.starts_with(&pre)).cloned().collect();
                                for k in gone {
                                    g.files.remove(&k);
                                    g.trash.push(k);
                                }
                                g.dirs.retain(|d| d != &rp && !d.starts_with(&pre));
                                let v = g.bump();
                                j(200, serde_json::json!({"version": v}))
                            }
                            None => err(404, "not_found", None),
                            Some(f) if if_match.as_deref() != Some("*") && if_match != Some(f.version.to_string()) => err(409, "conflict", Some(row(&rp, &f))),
                            Some(_) => {
                                g.files.remove(&rp);
                                g.trash.push(rp);
                                let v = g.bump();
                                j(200, serde_json::json!({"version": v}))
                            }
                        }
                    }
                }
                ("POST", "/api/v1/files/move") => {
                    let b: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
                    let (from, to) = (b["from"].as_str().unwrap_or("").to_string(), b["to"].as_str().unwrap_or("").to_string());
                    if !encrypted(&from) || !encrypted(&to) {
                        refuse(&mut g, 409, "e2ee_path_required")
                    } else {
                        let want = b["if_match"].as_i64().map(|v| v.to_string()).or(if_match.clone());
                        match g.files.get(&from).cloned() {
                            None => err(404, "not_found", None),
                            Some(f) if want.is_some() && want != Some(f.version.to_string()) => err(409, "conflict", Some(row(&from, &f))),
                            Some(_) if g.files.contains_key(&to) => err(409, "target_exists", None),
                            Some(mut f) => {
                                g.files.remove(&from);
                                f.version = g.bump();
                                let v = f.version;
                                let r = row(&to, &f);
                                g.files.insert(to, f);
                                j(200, serde_json::json!({"version": v, "moved": [{"file": r}]}))
                            }
                        }
                    }
                }
                ("GET", "/search") => j(200, serde_json::json!([])).with_header(Header::from_bytes("X-NullDrive-Search", "encrypted").unwrap()),
                _ => err(404, "not_found", None),
            };
            let _ = req.respond(resp);
        }
    });
    Mock { url, s }
}
