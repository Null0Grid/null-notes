#!/usr/bin/env python3
"""Stand-in for Null Drive's backup API (null-grid/notes-backup-drive-api.md),
for trying the Null Notes backup without a real server.

    python3 tools/mock_drive.py 8765 mytoken      # then use http://127.0.0.1:8765
    GET /_state  shows what was received (no auth, test only)
"""
import hashlib, json, sys, urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT, TOKEN = int(sys.argv[1]), sys.argv[2]
FILES, DIRS, TRASH, LOG = {}, set(), [], []

class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def j(self, code, obj):
        b = json.dumps(obj).encode()
        self.send_response(code); self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b))); self.end_headers(); self.wfile.write(b)
    def body(self):
        if "chunked" in (self.headers.get("Transfer-Encoding") or "").lower():
            out = b""
            while True:
                size = int(self.rfile.readline().split(b";")[0].strip() or b"0", 16)
                if size == 0:
                    self.rfile.readline(); return out
                out += self.rfile.read(size); self.rfile.readline()
        n = int(self.headers.get("Content-Length") or 0)
        return self.rfile.read(n) if n else b""
    def route(self):
        u = urllib.parse.urlsplit(self.path)
        if u.path == "/_state":
            return self.j(200, {"files": {k: {"size": len(v["data"]), "mtime": v["mtime"], "versions": v["versions"], "text": v["data"][:80].decode("utf8", "replace")} for k, v in sorted(FILES.items())}, "dirs": sorted(DIRS), "trash": TRASH, "log": LOG[-60:]})
        if self.headers.get("Authorization") != f"Bearer {TOKEN}":
            return self.j(401, {"error": "unauthorized"})
        LOG.append(f"{self.command} {urllib.parse.unquote(u.path)}")
        p = urllib.parse.unquote(u.path)
        if p == "/api/v1/me":
            return self.j(200, {"user": {"name": "luke", "e2ee": "off"}})
        if self.command == "POST" and p == "/api/v1/folders":
            d = json.loads(self.body())["path"]; new = d not in DIRS; DIRS.add(d)
            return self.j(201 if new else 200, {"path": d})
        if self.command == "GET" and p == "/api/v1/files":
            pre = urllib.parse.parse_qs(u.query).get("prefix", [""])[0] + "/"
            out = [{"path": k, "is_dir": False, "size": len(v["data"]), "sha256": v["sha"], "mtime": v["mtime"]} for k, v in FILES.items() if k.startswith(pre)]
            out += [{"path": d, "is_dir": True, "size": 0, "sha256": ""} for d in DIRS if d.startswith(pre)]
            return self.j(200, {"version": len(LOG), "files": out})
        if p.startswith("/api/v1/files/") and self.command == "PUT":
            rp = p[len("/api/v1/files/"):]
            if self.headers.get("If-Match") != "*": return self.j(428, {"error": "precondition_required"})
            data = self.body(); sha = hashlib.sha256(data).hexdigest()
            if self.headers.get("X-Content-SHA256") not in (None, sha): return self.j(400, {"error": "sha256_mismatch"})
            v = FILES.get(rp, {"versions": 0})
            FILES[rp] = {"data": data, "sha": sha, "mtime": self.headers.get("X-NullDrive-Mtime"), "versions": v["versions"] + 1}
            parts = rp.split("/")[:-1]
            for i in range(1, len(parts) + 1): DIRS.add("/".join(parts[:i]))
            return self.j(200 if v["versions"] else 201, {})
        if p.startswith("/api/v1/files/") and self.command == "DELETE":
            rp = p[len("/api/v1/files/"):]
            gone = [k for k in FILES if k == rp or k.startswith(rp + "/")]
            if not gone and rp not in DIRS: return self.j(404, {"error": "not_found"})
            for k in gone: FILES.pop(k); TRASH.append(k)
            if rp in DIRS: DIRS.discard(rp); TRASH.append(rp + "/")
            for d in [d for d in DIRS if d.startswith(rp + "/")]: DIRS.discard(d)
            return self.j(200, {"deleted": gone})
        if self.command == "POST" and p == "/api/v1/files/move":
            b = json.loads(self.body()); f, t = b["from"], b["to"]
            for k in [k for k in FILES if k == f or k.startswith(f + "/")]: FILES[t + k[len(f):]] = FILES.pop(k)
            for d in [d for d in DIRS if d == f or d.startswith(f + "/")]: DIRS.discard(d); DIRS.add(t + d[len(f):])
            return self.j(200, {})
        return self.j(404, {"error": "not_found"})
    do_GET = do_POST = do_PUT = do_DELETE = route

ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
