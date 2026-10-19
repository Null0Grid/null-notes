#!/usr/bin/env python3
"""Null Drive end-to-end encryption: an independent reference client.

Copied (minus the live-server demo) from the Project (null-grid/notes-e2ee-reference-client.py, Null Drive's
scripts/e2ee-reference/nulldrive_e2ee.py). Null Notes uses it as the *other*
implementation in its interop tests: what Notes writes, this client must read.

It follows docs/NOTES-E2EE.md step by step and shares no code with the Go
(shared/e2ee) and TypeScript (ui/src/lib/crypto) implementations, to show
the spec is enough for a third-party client such as Null Notes. It is
deliberately small and dependency-light: PyNaCl (libsodium) for
XChaCha20-Poly1305 and Argon2id, the standard library for everything else.

    pip install pynacl
    python3 nulldrive_e2ee.py vectors          # check against shared/e2ee/testdata/*.json
    python3 nulldrive_e2ee.py live URL         # full flow against a fresh server (see --help)

Not a product: no streaming, no retries, whole files in memory.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import hmac
import json
import os
import sys
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass

from nacl import bindings as sodium

# ---------------------------------------------------------------- primitives

KEY = 32
NONCE = 24  # XChaCha20-Poly1305
TAG = 16
CHUNK_VERSION = 0x01
CHUNK_OVERHEAD = 1 + NONCE + TAG  # 41
NAME_PAD = 16
MAX_NAME = 255
CROCKFORD = "0123456789ABCDEFGHJKMNPQRSTVWXYZ"


def hkdf_sha256(ikm: bytes, salt: bytes | None, info: bytes, length: int = KEY) -> bytes:
    """RFC 5869. An absent salt is HashLen zero bytes."""
    prk = hmac.new(salt or b"\x00" * 32, ikm, hashlib.sha256).digest()
    out, t, i = b"", b"", 1
    while len(out) < length:
        t = hmac.new(prk, t + info + bytes([i]), hashlib.sha256).digest()
        out += t
        i += 1
    return out[:length]


def aead_seal(key: bytes, nonce: bytes, pt: bytes, ad: bytes) -> bytes:
    return sodium.crypto_aead_xchacha20poly1305_ietf_encrypt(pt, ad, nonce, key)


def aead_open(key: bytes, nonce: bytes, ct: bytes, ad: bytes) -> bytes:
    return sodium.crypto_aead_xchacha20poly1305_ietf_decrypt(ct, ad, nonce, key)


def siv_seal(enc: bytes, siv: bytes, ad: bytes, pt: bytes) -> bytes:
    nonce = hmac.new(siv, ad + b"\x00" + pt, hashlib.sha256).digest()[:NONCE]
    return nonce + aead_seal(enc, nonce, pt, ad)


def siv_open(enc: bytes, siv: bytes, ad: bytes, blob: bytes) -> bytes:
    if len(blob) < NONCE + TAG:
        raise ValueError("too short")
    nonce = blob[:NONCE]
    pt = aead_open(enc, nonce, blob[NONCE:], ad)
    if not hmac.compare_digest(nonce, hmac.new(siv, ad + b"\x00" + pt, hashlib.sha256).digest()[:NONCE]):
        raise ValueError("synthetic nonce mismatch")
    return pt


def b64url(b: bytes) -> str:
    return base64.urlsafe_b64encode(b).rstrip(b"=").decode()


def unb64url(s: str) -> bytes:
    return base64.urlsafe_b64decode(s + "=" * (-len(s) % 4))


# ---------------------------------------------------------------- keys


class Keys:
    PURPOSES = ("name-enc", "name-siv", "chunk-enc", "chunk-siv", "file-hash", "auth", "fingerprint")

    def __init__(self, master: bytes):
        assert len(master) == KEY
        self.master = master
        self.sub = {p: hkdf_sha256(master, None, b"nulldrive/v1/" + p.encode()) for p in self.PURPOSES}

    @property
    def fingerprint(self) -> str:
        return self.sub["fingerprint"][:8].hex()

    @property
    def auth_key(self) -> str:
        return self.sub["auth"].hex()

    # names
    def encrypt_name(self, name: str) -> str:
        raw = name.encode("utf-8")
        if not raw or len(raw) > MAX_NAME or "/" in name:
            raise ValueError("bad name")
        padded = raw + b"\x80"
        padded += b"\x00" * (-len(padded) % NAME_PAD)
        return "~" + b64url(siv_seal(self.sub["name-enc"], self.sub["name-siv"], b"nulldrive/v1/name", padded))

    def decrypt_name(self, seg: str) -> str:
        if not seg.startswith("~"):
            raise ValueError("not an encrypted name")
        padded = siv_open(self.sub["name-enc"], self.sub["name-siv"], b"nulldrive/v1/name", unb64url(seg[1:]))
        body = padded.rstrip(b"\x00")
        if not body.endswith(b"\x80") or len(body) < 2:
            raise ValueError("bad padding")
        return body[:-1].decode("utf-8")

    def encrypt_path(self, path: str) -> str:
        return "/".join(self.encrypt_name(s) for s in path.split("/"))

    def decrypt_path(self, path: str) -> str:
        return "/".join(self.decrypt_name(s) for s in path.split("/"))

    # content
    def seal_chunk(self, plain: bytes) -> bytes:
        return bytes([CHUNK_VERSION]) + siv_seal(self.sub["chunk-enc"], self.sub["chunk-siv"], b"nulldrive/v1/chunk", plain)

    def open_chunk(self, sealed: bytes) -> bytes:
        if not sealed or sealed[0] != CHUNK_VERSION:
            raise ValueError("unknown chunk version")
        return siv_open(self.sub["chunk-enc"], self.sub["chunk-siv"], b"nulldrive/v1/chunk", sealed[1:])

    def file_hash(self, plaintext: bytes) -> str:
        return hmac.new(self.sub["file-hash"], hashlib.sha256(plaintext).digest(), hashlib.sha256).hexdigest()


# ---------------------------------------------------------------- the bundle


def parse_recovery_key(s: str) -> bytes:
    acc, bits, out, n = 0, 0, bytearray(), 0
    for ch in s.upper():
        if ch in "- \t\r\n":
            continue
        ch = {"O": "0", "I": "1", "L": "1"}.get(ch, ch)
        i = CROCKFORD.index(ch)
        acc, bits, n = (acc << 5) | i, bits + 5, n + 1
        if bits >= 8:
            bits -= 8
            out.append((acc >> bits) & 0xFF)
    if n != 32 or len(out) != 20:
        raise ValueError("a recovery key is 32 characters")
    return bytes(out)


def format_recovery_key(b: bytes) -> str:
    n = int.from_bytes(b, "big")
    chars = "".join(CROCKFORD[(n >> (5 * (31 - i))) & 31] for i in range(32))
    return "-".join(chars[i : i + 4] for i in range(0, 32, 4))


def passphrase_kek(passphrase: str, kdf: dict) -> bytes:
    if kdf["alg"] != "argon2id" or kdf["threads"] != 1:
        raise ValueError("unsupported kdf")
    return sodium.crypto_pwhash_alg(
        KEY, passphrase.encode("utf-8"), base64.b64decode(kdf["salt"]),
        kdf["time"], kdf["memory_kib"] * 1024, sodium.crypto_pwhash_ALG_ARGON2ID13,
    )


def recovery_kek(rk: bytes, salt: bytes) -> bytes:
    return hkdf_sha256(rk, salt, b"nulldrive/v1/recovery-kek")


def unwrap(kek: bytes, blob: bytes, ad: bytes) -> bytes:
    if len(blob) != NONCE + KEY + TAG:
        raise ValueError("bad wrap")
    return aead_open(kek, blob[:NONCE], blob[NONCE:], ad)


def wrap(kek: bytes, master: bytes, ad: bytes) -> bytes:
    nonce = os.urandom(NONCE)
    return nonce + aead_seal(kek, nonce, master, ad)


def unlock(bundle: dict, passphrase: str | None = None, recovery_key: str | None = None) -> Keys:
    if bundle["version"] != 1:
        raise ValueError("unknown bundle version")
    if passphrase is not None:
        master = unwrap(passphrase_kek(passphrase, bundle["kdf"]), base64.b64decode(bundle["passphrase_wrapped"]),
                        b"nulldrive/v1/wrap/passphrase")
    else:
        rk = parse_recovery_key(recovery_key or "")
        master = unwrap(recovery_kek(rk, base64.b64decode(bundle["recovery_salt"])),
                        base64.b64decode(bundle["recovery_wrapped"]), b"nulldrive/v1/wrap/recovery")
    keys = Keys(master)
    if keys.fingerprint != bundle["fingerprint"]:
        raise ValueError("fingerprint mismatch")
    return keys


def new_bundle(passphrase: str, time_cost: int = 3, memory_kib: int = 65536) -> tuple[dict, Keys, str]:
    """Turning encryption on (only needed for the live demo: Null Notes
    joins accounts that already have it)."""
    keys = Keys(os.urandom(KEY))
    rk, rsalt = os.urandom(20), os.urandom(16)
    kdf = {"alg": "argon2id", "time": time_cost, "memory_kib": memory_kib, "threads": 1,
           "salt": base64.b64encode(os.urandom(16)).decode()}
    bundle = {
        "version": 1, "kdf": kdf,
        "passphrase_wrapped": base64.b64encode(wrap(passphrase_kek(passphrase, kdf), keys.master,
                                                    b"nulldrive/v1/wrap/passphrase")).decode(),
        "recovery_wrapped": base64.b64encode(wrap(recovery_kek(rk, rsalt), keys.master,
                                                  b"nulldrive/v1/wrap/recovery")).decode(),
        "recovery_salt": base64.b64encode(rsalt).decode(),
        "fingerprint": keys.fingerprint,
    }
    return bundle, keys, format_recovery_key(rk)


# ---------------------------------------------------------------- vectors


def check_vectors(testdata: str) -> int:
    fails = 0

    def eq(what, got, want):
        nonlocal fails
        if got != want:
            fails += 1
            print(f"  FAIL {what}\n    got  {got}\n    want {want}")
        else:
            print(f"  ok   {what}")

    v = json.load(open(os.path.join(testdata, "vectors.json")))
    print("vectors.json")
    k = Keys(bytes.fromhex(v["master"]))
    eq("fingerprint", k.fingerprint, v["fingerprint"])
    eq("auth_key", k.auth_key, v["auth_key"])
    for n in v["names"]:
        eq(f"name {n['Plain'][:20]!r}", k.encrypt_name(n["Plain"]), n["Enc"])
        eq("  decrypts", k.decrypt_name(n["Enc"]), n["Plain"])
    for p in v["paths"]:
        eq(f"path {p['Plain']!r}", k.encrypt_path(p["Plain"]), p["Enc"])
    for c in v["chunks"]:
        pt = bytes.fromhex(c["Plain"])
        eq(f"chunk ({len(pt)} bytes)", k.seal_chunk(pt).hex(), c["Sealed"])
        eq("  opens", k.open_chunk(bytes.fromhex(c["Sealed"])), pt)
    for fh in v["file_hashes"]:
        got = hmac.new(k.sub["file-hash"], bytes.fromhex(fh["SHA256"]), hashlib.sha256).hexdigest()
        eq(f"file hash of {fh['SHA256'][:12]}…", got, fh["Mapped"])
    eq("unlock with passphrase", unlock(v["bundle"], passphrase=v["passphrase"]).master.hex(), v["master"])
    eq("unlock with recovery key", unlock(v["bundle"], recovery_key=v["recovery_key"]).master.hex(), v["master"])
    eq("recovery key round trip", format_recovery_key(parse_recovery_key(v["recovery_key"])), v["recovery_key"])
    try:
        unlock(v["bundle"], passphrase="wrong passphrase")
        eq("wrong passphrase refused", "accepted", "refused")
    except Exception:
        eq("wrong passphrase refused", "refused", "refused")

    n = json.load(open(os.path.join(testdata, "notes-vector.json")))
    print("notes-vector.json")
    k = Keys(bytes.fromhex(n["master"]))
    for p, h in n["subkeys"].items():
        eq(f"subkey {p}", k.sub[p].hex(), h)
    eq("auth_hash (what the server stores)", hashlib.sha256(k.auth_key.encode()).hexdigest(), n["auth_hash"])
    for s in n["segments"]:
        eq(f"segment {s['plain']!r}", k.encrypt_name(s["plain"]), s["enc"])
    eq("encrypted path", k.encrypt_path(n["path"]), n["encrypted_path"])
    eq("decrypted path", k.decrypt_path(n["encrypted_path"]), n["path"])
    content = n["content"].encode()
    eq("content sha256", hashlib.sha256(content).hexdigest(), n["content_sha256"])
    eq("file hash", k.file_hash(content), n["file_hash"])
    sealed = k.seal_chunk(content)
    eq("sealed chunk", sealed.hex(), n["sealed_chunk"])
    eq("chunk hash", hashlib.sha256(sealed).hexdigest(), n["chunk_hash"])
    commit = {"size": len(content), "sha256": k.file_hash(content), "mtime": "2026-10-18T08:00:00Z",
              "chunks": [{"hash": hashlib.sha256(sealed).hexdigest(), "size": len(sealed)}]}
    eq("commit body", commit, n["commit"])
    print("all vectors match" if not fails else f"{fails} mismatches")
    return 1 if fails else 0


# ---------------------------------------------------------------- HTTP


class HTTPError(Exception):
    def __init__(self, status: int, body: dict, headers):
        self.status, self.body, self.headers = status, body, headers
        self.code = (body.get("error") or {}).get("code", "")
        super().__init__(f"{status} {self.code}")


@dataclass
class Drive:
    base: str
    token: str
    keys: Keys | None = None
    chunk_size: int = 4 << 20  # any size up to 16 MiB of ciphertext works

    def call(self, method, path, body=None, headers=None, raw=False, e2ee=True):
        h = {"Authorization": "Bearer " + self.token}
        if e2ee:
            h["X-NullDrive-E2EE"] = "1"
        data = None
        if isinstance(body, (bytes, bytearray)):
            data = bytes(body)
        elif body is not None:
            data, h["Content-Type"] = json.dumps(body).encode(), "application/json"
        h.update(headers or {})
        req = urllib.request.Request(self.base.rstrip("/") + path, data=data, method=method, headers=h)
        try:
            with urllib.request.urlopen(req) as r:
                out = r.read()
                return (out if raw else json.loads(out or b"null")), r.status, r.headers
        except urllib.error.HTTPError as e:
            b = e.read()
            try:
                j = json.loads(b)
            except ValueError:
                j = {}
            raise HTTPError(e.code, j, e.headers) from None

    @staticmethod
    def q(path: str) -> str:
        return "/".join(urllib.parse.quote(s, safe="") for s in path.split("/"))

    def join(self, passphrase=None, recovery_key=None) -> dict:
        st, _, _ = self.call("GET", "/api/v2/e2ee")
        if st["state"] == "off":
            raise RuntimeError("account is not encrypted: use the plain API")
        self.keys = unlock(st["bundle"], passphrase, recovery_key)
        return st

    def decrypt_row(self, f: dict) -> dict:
        f = dict(f)
        try:
            f["plain_path"] = self.keys.decrypt_path(f["path"])
            if f.get("moved_to"):
                f["plain_moved_to"] = self.keys.decrypt_path(f["moved_to"])
        except Exception:
            f["plain_path"] = None  # a migration leftover (plaintext) or someone else's key: skip it
        return f

    def list(self, prefix: str | None = None, since: int | None = None) -> dict:
        q = {}
        if prefix:
            q["prefix"] = self.keys.encrypt_path(prefix)
        if since is not None:
            q["since"] = str(since)
        fl, _, _ = self.call("GET", "/api/v1/files" + ("?" + urllib.parse.urlencode(q) if q else ""))
        fl["files"] = [self.decrypt_row(f) for f in fl["files"]]
        return fl

    def upload(self, path: str, content: bytes, mtime: str | None = None, if_match=None, if_none_match=False) -> dict:
        k = self.keys
        # An empty file has no chunks (a chunk must hold at least one byte).
        pieces = [content[i : i + self.chunk_size] for i in range(0, len(content), self.chunk_size)]
        sealed = [k.seal_chunk(p) for p in pieces]
        refs = [{"hash": hashlib.sha256(s).hexdigest(), "size": len(s)} for s in sealed]
        if refs:
            missing, _, _ = self.call("POST", "/api/v2/chunks/check", {"hashes": [r["hash"] for r in refs]})
            for r, s in zip(refs, sealed):
                if r["hash"] in missing["missing"]:
                    self.call("PUT", "/api/v2/chunks/" + r["hash"], s)
        body = {"size": len(content), "sha256": k.file_hash(content), "chunks": refs}
        if mtime:
            body["mtime"] = mtime
        h = {}
        if if_none_match:
            h["If-None-Match"] = "*"
        elif if_match is not None:
            h["If-Match"] = str(if_match)
        f, _, _ = self.call("PUT", "/api/v2/files/" + self.q(k.encrypt_path(path)), body, h)
        return self.decrypt_row(f)

    def download(self, path: str) -> tuple[bytes, dict]:
        m, _, _ = self.call("GET", "/api/v2/files/" + self.q(self.keys.encrypt_path(path)) + "/manifest")
        out = b"".join(self.keys.open_chunk(self.call("GET", "/api/v2/chunks/" + c["hash"], raw=True)[0])
                       for c in m["chunks"])
        if len(out) != m["size"] or self.keys.file_hash(out) != m["sha256"]:
            raise ValueError("content does not match the manifest")
        return out, m

    def move(self, src: str, dst: str, if_match: int) -> dict:
        r, _, _ = self.call("POST", "/api/v1/files/move",
                            {"from": self.keys.encrypt_path(src), "to": self.keys.encrypt_path(dst)},
                            {"If-Match": str(if_match)})
        return r

    def delete(self, path: str, if_match: int) -> dict:
        r, _, _ = self.call("DELETE", "/api/v1/files/" + self.q(self.keys.encrypt_path(path)), None,
                            {"If-Match": str(if_match)})
        return r


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    v = sub.add_parser("vectors")
    v.add_argument("testdata", nargs="?", default=os.path.join(os.path.dirname(__file__), "..", "..", "src-tauri",
                                                                "testdata", "e2ee"))
    a = ap.parse_args()
    return check_vectors(a.testdata)


if __name__ == "__main__":
    sys.exit(main())
