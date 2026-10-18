#!/usr/bin/env python3
"""Stand-in for Null Drive's backup + two-way sync API, for trying Null Notes
without a real server. Semantics per the Project docs
null-grid/notes-backup-drive-api.md and null-grid/notes-sync-drive-api.md:
versions are ETags, If-Match "<v>" / If-None-Match: * / If-Match: *,
409 conflict with the current file, 428 without a precondition, moves keep id.

    python3 tools/mock_drive.py 8765 mytoken      # then use http://127.0.0.1:8765
    GET  /_state              what Drive holds (no auth, test only)
    POST /_edit?path=…        replace a file as "another device" would (body = new bytes)
"""
import hashlib, json, re, sys, threading, urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT, TOKEN = int(sys.argv[1]), sys.argv[2]
FILES, DIRS, TRASH, LOG = {}, set(), [], []
COUNTER = [0]
LOCK = threading.Lock()


def bump():
    COUNTER[0] += 1
    return COUNTER[0]


def sha(b):
    return hashlib.sha256(b).hexdigest()


def synced_id(path, data):
    if not path.lower().endswith((".md", ".markdown")):
        return None
    m = re.match(rb"---\r?\n(.*?)\r?\n(---|\.\.\.)\r?\n", data, re.S)
    if not m:
        return None
    for line in m.group(1).splitlines():
        if line.startswith(b"id:"):
            return line[3:].strip().strip(b"\"'").decode().lower() or None
    return None


def row(p, f):
    r = {"id": f["id"], "path": p, "is_dir": False, "size": len(f["data"]), "sha256": sha(f["data"]),
         "version": f["version"], "deleted": False, "mtime": f.get("mtime")}
    sid = synced_id(p, f["data"])
    if sid:
        r["synced_id"] = sid
    return r


def put_file(p, data, mtime=None):
    old = FILES.get(p)
    FILES[p] = {"id": old["id"] if old else "%032x" % (COUNTER[0] + 1), "data": data, "mtime": mtime,
                "version": bump(), "versions": (old["versions"] if old else 0) + 1}
    parts = p.split("/")[:-1]
    for i in range(1, len(parts) + 1):
        DIRS.add("/".join(parts[:i]))
    return FILES[p]


class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def j(self, code, obj, etag=None):
        b = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b)))
        if etag is not None:
            self.send_header("ETag", '"%s"' % etag)
        self.end_headers()
        self.wfile.write(b)

    def err(self, code, c, current=None):
        self.j(code, {"error": {"code": c, "message": c, "current": current}}, current and current.get("version"))

    def body(self):
        if "chunked" in (self.headers.get("Transfer-Encoding") or "").lower():
            out = b""
            while True:
                size = int(self.rfile.readline().split(b";")[0].strip() or b"0", 16)
                if size == 0:
                    self.rfile.readline()
                    return out
                out += self.rfile.read(size)
                self.rfile.readline()
        n = int(self.headers.get("Content-Length") or 0)
        return self.rfile.read(n) if n else b""

    def route(self):
        with LOCK:
            self._route()

    def _route(self):
        u = urllib.parse.urlsplit(self.path)
        p = urllib.parse.unquote(u.path)
        q = urllib.parse.parse_qs(u.query)
        if p == "/_state":
            return self.j(200, {"files": {k: {"size": len(v["data"]), "mtime": v.get("mtime"), "version": v["version"], "versions": v["versions"], "id": v["id"],
                                               "text": v["data"][:200].decode("utf8", "replace")} for k, v in sorted(FILES.items())},
                                "dirs": sorted(DIRS), "trash": TRASH, "log": LOG[-80:]})
        if p == "/_edit":
            data = self.body()
            return self.j(200, row(q["path"][0], put_file(q["path"][0], data)))
        if self.headers.get("Authorization") != f"Bearer {TOKEN}":
            return self.err(401, "unauthorized")
        LOG.append(f"{self.command} {p}")
        if p == "/api/v1/me":
            return self.j(200, {"user": {"name": "luke", "e2ee": "off"}})
        if self.command == "POST" and p == "/api/v1/folders":
            d = json.loads(self.body())["path"]
            new = d not in DIRS
            DIRS.add(d)
            return self.j(201 if new else 200, {"path": d})
        if self.command == "GET" and p == "/api/v1/files":
            pre = q.get("prefix", [""])[0] + "/"
            out = [row(k, v) for k, v in sorted(FILES.items()) if k.startswith(pre)]
            out += [{"path": d, "is_dir": True, "size": 0, "sha256": "", "version": 0} for d in sorted(DIRS) if d.startswith(pre)]
            return self.j(200, {"version": COUNTER[0], "files": out})
        if p == "/api/v1/files/move" and self.command == "POST":
            b = json.loads(self.body())
            f, t = b["from"], b["to"]
            want = b.get("if_match")
            hm = (self.headers.get("If-Match") or "").strip('"')
            if want is None and hm:
                want = int(hm)
            cur = FILES.get(f)
            if cur is None:
                return self.err(404, "not_found")
            if want is not None and want != cur["version"]:
                return self.err(409, "conflict", row(f, cur))
            if t in FILES:
                return self.err(409, "target_exists")
            v = bump()
            for k in [k for k in FILES if k == f or k.startswith(f + "/")]:
                FILES[t + k[len(f):]] = FILES.pop(k)
                FILES[t + k[len(f):]]["version"] = v
            parts = t.split("/")[:-1]
            for i in range(1, len(parts) + 1):
                DIRS.add("/".join(parts[:i]))
            return self.j(200, {"version": v})
        if not p.startswith("/api/v1/files/"):
            return self.err(404, "not_found")
        rp = p[len("/api/v1/files/"):]
        cur = FILES.get(rp)
        if self.command == "GET":
            if cur is None:
                return self.err(404, "not_found")
            self.send_response(200)
            self.send_header("Content-Length", str(len(cur["data"])))
            self.send_header("ETag", '"%d"' % cur["version"])
            self.send_header("X-Content-SHA256", sha(cur["data"]))
            self.send_header("X-NullDrive-File-Id", cur["id"])
            self.end_headers()
            self.wfile.write(cur["data"])
            return
        im, inm = self.headers.get("If-Match"), self.headers.get("If-None-Match")
        if self.command == "PUT":
            data = self.body()
            claimed = self.headers.get("X-Content-SHA256")
            if claimed and claimed != sha(data):
                return self.err(400, "sha256_mismatch")
            same = cur is not None and sha(cur["data"]) == sha(data)
            if im and inm:
                return self.err(400, "invalid_if_match")
            if inm is not None:
                ok = cur is None or same
            elif im == "*":
                ok = True
            elif im:
                ok = cur is not None and (int(im.strip('"')) == cur["version"] or same)
            else:
                if cur is not None:
                    return self.err(428, "precondition_required")
                ok = True
            if not ok:
                return self.err(409, "conflict", row(rp, cur) if cur else None)
            if same:
                if self.headers.get("X-NullDrive-Mtime"):
                    cur["mtime"] = self.headers.get("X-NullDrive-Mtime")
                return self.j(200, row(rp, cur), cur["version"])
            f = put_file(rp, data, self.headers.get("X-NullDrive-Mtime"))
            return self.j(200 if cur else 201, row(rp, f), f["version"])
        if self.command == "DELETE":
            gone = [k for k in FILES if k == rp or k.startswith(rp + "/")]
            if not gone and rp not in DIRS:
                return self.err(404, "not_found")
            if cur is not None and im and im != "*" and int(im.strip('"')) != cur["version"]:
                return self.err(409, "conflict", row(rp, cur))
            for k in gone:
                FILES.pop(k)
                TRASH.append(k)
            if rp in DIRS:
                DIRS.discard(rp)
                TRASH.append(rp + "/")
            for d in [d for d in DIRS if d.startswith(rp + "/")]:
                DIRS.discard(d)
            return self.j(200, {"version": bump(), "deleted": gone})
        return self.err(404, "not_found")

    do_GET = do_POST = do_PUT = do_DELETE = route


ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
