#!/usr/bin/env python3
"""Stand-in for an end-to-end encrypted Null Drive account (server 0.8.3+),
for trying Null Notes' Phase 8 without a real server. Like the real server
it never has the key; it enforces what null-grid/notes-e2ee-drive-api.md
says the server enforces:

  X-NullDrive-E2EE: 1 on listings and v2 calls      (409 e2ee_client_required)
  no v1 whole-file PUT/GET                           (409 e2ee_required)
  every path segment encrypted (~…)                  (409 e2ee_path_required)
  chunks verified by SHA-256                         (400 hash_mismatch)
  commits: chunks stored (422 missing_chunks), sizes - 41 each = size (400 invalid_manifest)
  versions, If-Match / If-None-Match: *, 409 conflict + current, 428, moves keep the id
  no synced_id; search answers [] with X-NullDrive-Search: encrypted

The account key bundle is the test bundle from vectors.json:
passphrase "vector passphrase 123", recovery key B9D5-MPJT-B9D5-MPJT-B9D5-MPJT-B9D5-MPJT.

    python3 tools/mock_drive_e2ee.py 8766 mytoken     # then use http://127.0.0.1:8766
    GET /_state     what Drive holds (encrypted, as the server sees it; no auth, test only)
"""
import hashlib, json, os, sys, threading, urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT, TOKEN = int(sys.argv[1]), sys.argv[2]
HERE = os.path.dirname(os.path.abspath(__file__))
V = json.load(open(os.path.join(HERE, "..", "src-tauri", "testdata", "e2ee", "vectors.json")))
BUNDLE, FINGERPRINT = V["bundle"], V["fingerprint"]
FILES, DIRS, CHUNKS, TRASH, LOG = {}, set(), {}, [], []
COUNTER = [0]
LOCK = threading.Lock()


def bump():
    COUNTER[0] += 1
    return COUNTER[0]


def enc(p):
    return bool(p) and all(s.startswith("~") and len(s) >= 76 for s in p.split("/"))


def row(p, f):
    return {"id": f["id"], "path": p, "is_dir": False, "size": f["size"], "sha256": f["sha256"], "version": f["version"], "deleted": False}


class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def j(self, code, obj, etag=None, extra=None):
        b = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b)))
        if etag is not None:
            self.send_header("ETag", '"%s"' % etag)
        for k, v in (extra or {}).items():
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(b)

    def err(self, code, c, current=None):
        self.j(code, {"error": {"code": c, "message": c, "current": current}}, current and current.get("version"))

    def body(self):
        n = int(self.headers.get("Content-Length") or 0)
        return self.rfile.read(n) if n else b""

    def route(self):
        with LOCK:
            self._route()

    def _route(self):
        u = urllib.parse.urlsplit(self.path)
        p = urllib.parse.unquote(u.path)
        q = urllib.parse.parse_qs(u.query)
        data = self.body()
        if p == "/_state":
            return self.j(200, {"files": FILES, "dirs": sorted(DIRS), "chunks": len(CHUNKS), "trash": TRASH, "log": LOG[-200:]})
        if self.headers.get("Authorization") != f"Bearer {TOKEN}":
            return self.err(401, "unauthorized")
        LOG.append(f"{self.command} {p}")
        e2 = self.headers.get("X-NullDrive-E2EE") == "1"
        if (p == "/api/v1/files" or (p.startswith("/api/v2/") and p != "/api/v2/e2ee")) and not e2:
            return self.err(409, "e2ee_client_required")
        im, inm = self.headers.get("If-Match"), self.headers.get("If-None-Match")
        im = im.strip('"') if im else None
        cmd = self.command
        if p == "/api/v1/me":
            return self.j(200, {"user": {"name": "luke", "e2ee": "on"}})
        if p == "/api/v2/e2ee":
            return self.j(200, {"state": "on", "fingerprint": FINGERPRINT, "enabled_at": "2026-10-18T07:12:00Z", "plaintext_files": 0, "bundle": BUNDLE})
        if p == "/search":
            return self.j(200, [], extra={"X-NullDrive-Search": "encrypted"})
        if p.startswith("/api/v1/files/") and cmd in ("GET", "PUT", "HEAD"):
            return self.err(409, "e2ee_required")
        if cmd == "POST" and p == "/api/v1/folders":
            d = json.loads(data)["path"]
            if not enc(d):
                return self.err(409, "e2ee_path_required")
            segs = d.split("/")
            for i in range(1, len(segs) + 1):
                DIRS.add("/".join(segs[:i]))
            return self.j(201, {"path": d})
        if cmd == "GET" and p == "/api/v1/files":
            pre = q.get("prefix", [""])[0]
            if pre and not enc(pre):
                return self.err(409, "e2ee_path_required")
            pre += "/"
            rows = [row(k, f) for k, f in sorted(FILES.items()) if k.startswith(pre)]
            rows += [{"id": "dir", "path": d, "is_dir": True, "size": 0, "sha256": "", "version": 0} for d in sorted(DIRS) if d.startswith(pre)]
            return self.j(200, {"version": COUNTER[0], "files": rows})
        if cmd == "POST" and p == "/api/v2/chunks/check":
            return self.j(200, {"missing": [h for h in json.loads(data)["hashes"] if h not in CHUNKS]})
        if p.startswith("/api/v2/chunks/"):
            h = p[len("/api/v2/chunks/"):]
            if cmd == "PUT":
                if hashlib.sha256(data).hexdigest() != h:
                    return self.err(400, "hash_mismatch")
                if not data or data[0] != 1 or len(data) > 16 << 20:
                    return self.err(400, "invalid_chunk")
                CHUNKS[h] = data
                return self.j(201, {"hash": h, "size": len(data)})
            if h not in CHUNKS:
                return self.err(404, "not_found")
            self.send_response(200)
            self.send_header("Content-Length", str(len(CHUNKS[h])))
            self.end_headers()
            self.wfile.write(CHUNKS[h])
            return
        if p.startswith("/api/v2/files/") and p.endswith("/manifest") and cmd == "GET":
            rp = p[len("/api/v2/files/"):-len("/manifest")]
            f = FILES.get(rp)
            if not f:
                return self.err(404, "not_found")
            return self.j(200, {"path": rp, "id": f["id"], "version": f["version"], "size": f["size"], "sha256": f["sha256"], "mtime": f.get("mtime"),
                                "chunks": [{"hash": h, "size": n} for h, n in f["chunks"]]}, f["version"])
        if p.startswith("/api/v2/files/") and cmd == "PUT":
            rp = p[len("/api/v2/files/"):]
            if not enc(rp):
                return self.err(409, "e2ee_path_required")
            b = json.loads(data)
            chunks = [(c["hash"], c["size"]) for c in b.get("chunks", [])]
            if sum(n - 41 for _, n in chunks) != b.get("size") or len(b.get("sha256", "")) != 64:
                return self.err(400, "invalid_manifest")
            missing = [h for h, _ in chunks if h not in CHUNKS]
            if missing:
                return self.j(422, {"error": {"code": "missing_chunks", "message": "missing_chunks"}, "missing": missing})
            cur = FILES.get(rp)
            same = cur is not None and cur["sha256"] == b["sha256"] and cur["size"] == b["size"]
            if inm is not None:
                ok = cur is None or same
            elif im == "*":
                ok = True
            elif im:
                ok = cur is not None and (str(cur["version"]) == im or same)
            else:
                if cur is not None:
                    return self.err(428, "precondition_required")
                ok = True
            if not ok:
                return self.err(409, "conflict", row(rp, cur) if cur else None)
            if same:
                return self.j(200, row(rp, cur), cur["version"])
            v = bump()
            f = {"id": cur["id"] if cur else "%032x" % v, "version": v, "size": b["size"], "sha256": b["sha256"], "chunks": chunks, "mtime": b.get("mtime")}
            FILES[rp] = f
            segs = rp.split("/")
            for i in range(1, len(segs)):
                DIRS.add("/".join(segs[:i]))
            return self.j(200 if cur else 201, row(rp, f), v)
        if cmd == "DELETE" and p.startswith("/api/v1/files/"):
            rp = p[len("/api/v1/files/"):]
            if not enc(rp):
                return self.err(409, "e2ee_path_required")
            cur = FILES.get(rp)
            if cur is None and rp not in DIRS:
                return self.err(404, "not_found")
            if cur is not None and im and im != "*" and im != str(cur["version"]):
                return self.err(409, "conflict", row(rp, cur))
            for k in [k for k in FILES if k == rp or k.startswith(rp + "/")]:
                FILES.pop(k)
                TRASH.append(k)
            for d in [d for d in DIRS if d == rp or d.startswith(rp + "/")]:
                DIRS.discard(d)
            return self.j(200, {"version": bump()})
        if cmd == "POST" and p == "/api/v1/files/move":
            b = json.loads(data)
            fr, to = b["from"], b["to"]
            if not enc(fr) or not enc(to):
                return self.err(409, "e2ee_path_required")
            want = b.get("if_match")
            if want is None and im:
                want = int(im)
            cur = FILES.get(fr)
            if cur is None:
                return self.err(404, "not_found")
            if want is not None and want != cur["version"]:
                return self.err(409, "conflict", row(fr, cur))
            if to in FILES:
                return self.err(409, "target_exists")
            cur["version"] = bump()
            FILES[to] = FILES.pop(fr)
            segs = to.split("/")
            for i in range(1, len(segs)):
                DIRS.add("/".join(segs[:i]))
            return self.j(200, {"version": cur["version"], "moved": [{"file": row(to, cur)}]})
        return self.err(404, "not_found")

    do_GET = do_POST = do_PUT = do_DELETE = do_HEAD = route


ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
