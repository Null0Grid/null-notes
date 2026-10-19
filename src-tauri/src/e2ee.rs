//! End-to-end encryption for Null Drive accounts that have it (Phase 8).
//!
//! Implements the client side of `null-grid/notes-e2ee-drive-api.md`
//! (Drive 0.8.3+), checked against Drive's own test vectors
//! (`testdata/e2ee/vectors.json`, `testdata/e2ee/notes-vector.json`):
//!
//! - the account key: Argon2id (passphrase) or HKDF (recovery key) unwraps
//!   the 32-byte master key with XChaCha20-Poly1305; seven HKDF-SHA256
//!   subkeys; the fingerprint is checked and pinned.
//! - names: each path segment ISO 7816-4 padded to 16 bytes, sealed with a
//!   deterministic SIV over XChaCha20-Poly1305, written `~` + base64url.
//! - content: chunks `0x01 ‖ SIV(…)`, named by SHA-256 of the sealed bytes;
//!   the file's `sha256` on Drive is `HMAC(file-hash, SHA-256(plaintext))`.
//!
//! Nothing here talks to the network; `backup::Drive` uses it.

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use zeroize::Zeroize;

type HmacSha256 = Hmac<Sha256>;

pub const KEY: usize = 32;
const NONCE: usize = 24;
const TAG: usize = 16;
const CHUNK_VERSION: u8 = 0x01;
/// Bytes a sealed chunk adds to its piece: version + nonce + tag.
#[allow(dead_code)]
pub const CHUNK_OVERHEAD: usize = 1 + NONCE + TAG;
/// Largest sealed chunk Drive accepts.
#[allow(dead_code)]
pub const MAX_SEALED: usize = 16 * 1024 * 1024;
/// Notes' piece size for anything big (the spec allows any split).
pub const PIECE: usize = 4 * 1024 * 1024;
const NAME_PAD: usize = 16;
const MAX_NAME: usize = 255;
const CROCKFORD: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

const AD_NAME: &[u8] = b"nulldrive/v1/name";
const AD_CHUNK: &[u8] = b"nulldrive/v1/chunk";
const AD_WRAP_PASS: &[u8] = b"nulldrive/v1/wrap/passphrase";
const AD_WRAP_RECOVERY: &[u8] = b"nulldrive/v1/wrap/recovery";

/* ======================= primitives ======================= */

fn hmac256(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut m = <HmacSha256 as Mac>::new_from_slice(key).expect("HMAC takes any key length");
    for p in parts {
        m.update(p);
    }
    m.finalize().into_bytes().into()
}

/// HKDF-SHA256 (RFC 5869), 32-byte output. No salt = 32 zero bytes.
pub fn hkdf(ikm: &[u8], salt: Option<&[u8]>, info: &[u8]) -> [u8; 32] {
    let prk = hmac256(salt.unwrap_or(&[0u8; 32]), &[ikm]);
    hmac256(&prk, &[info, &[1u8]]) // one block is all 32 bytes need
}

fn aead_seal(key: &[u8], nonce: &[u8], pt: &[u8], ad: &[u8]) -> Vec<u8> {
    XChaCha20Poly1305::new(key.into()).encrypt(XNonce::from_slice(nonce), Payload { msg: pt, aad: ad }).expect("XChaCha20-Poly1305 seal")
}

fn aead_open(key: &[u8], nonce: &[u8], ct: &[u8], ad: &[u8]) -> Result<Vec<u8>, E2eeErr> {
    XChaCha20Poly1305::new(key.into()).decrypt(XNonce::from_slice(nonce), Payload { msg: ct, aad: ad }).map_err(|_| E2eeErr::Decrypt)
}

fn siv_seal(enc: &[u8], siv: &[u8], ad: &[u8], pt: &[u8]) -> Vec<u8> {
    let nonce = &hmac256(siv, &[ad, &[0u8], pt])[..NONCE];
    let mut out = nonce.to_vec();
    out.extend(aead_seal(enc, nonce, pt, ad));
    out
}

fn siv_open(enc: &[u8], siv: &[u8], ad: &[u8], blob: &[u8]) -> Result<Vec<u8>, E2eeErr> {
    if blob.len() < NONCE + TAG {
        return Err(E2eeErr::Decrypt);
    }
    let pt = aead_open(enc, &blob[..NONCE], &blob[NONCE..], ad)?;
    let want = hmac256(siv, &[ad, &[0u8], &pt]);
    // constant time is not needed: the AEAD already authenticated everything
    if want[..NONCE] != blob[..NONCE] {
        return Err(E2eeErr::Decrypt);
    }
    Ok(pt)
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

#[derive(Debug, Clone, PartialEq)]
pub enum E2eeErr {
    /// AEAD open failed: wrong passphrase/key, or corrupt data
    Decrypt,
    /// a bundle or value we don't understand
    Format(String),
    /// the unlocked key isn't the account's (or not the one pinned)
    Fingerprint,
}

impl std::fmt::Display for E2eeErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            E2eeErr::Decrypt => write!(f, "couldn't decrypt"),
            E2eeErr::Format(m) => write!(f, "{m}"),
            E2eeErr::Fingerprint => write!(f, "the key's fingerprint doesn't match the account"),
        }
    }
}

/* ======================= keys ======================= */

#[allow(dead_code)]
pub const PURPOSES: [&str; 7] = ["name-enc", "name-siv", "chunk-enc", "chunk-siv", "file-hash", "auth", "fingerprint"];

/// The account's master key and its subkeys. Zeroed on drop.
pub struct Keys {
    master: [u8; KEY],
    name_enc: [u8; 32],
    name_siv: [u8; 32],
    chunk_enc: [u8; 32],
    chunk_siv: [u8; 32],
    file_hash: [u8; 32],
    fingerprint: [u8; 32],
    #[allow(dead_code)]
    auth: [u8; 32],
}

impl Drop for Keys {
    fn drop(&mut self) {
        for k in [&mut self.master, &mut self.name_enc, &mut self.name_siv, &mut self.chunk_enc, &mut self.chunk_siv, &mut self.file_hash, &mut self.fingerprint, &mut self.auth] {
            k.zeroize();
        }
    }
}

impl std::fmt::Debug for Keys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Keys({})", self.fingerprint())
    }
}

impl Keys {
    pub fn from_master(master: &[u8]) -> Result<Keys, E2eeErr> {
        let m: [u8; KEY] = master.try_into().map_err(|_| E2eeErr::Format("a master key is 32 bytes".into()))?;
        let sub = |p: &str| hkdf(&m, None, format!("nulldrive/v1/{p}").as_bytes());
        Ok(Keys {
            master: m,
            name_enc: sub("name-enc"),
            name_siv: sub("name-siv"),
            chunk_enc: sub("chunk-enc"),
            chunk_siv: sub("chunk-siv"),
            file_hash: sub("file-hash"),
            fingerprint: sub("fingerprint"),
            auth: sub("auth"),
        })
    }

    pub fn master_hex(&self) -> String {
        hex(&self.master)
    }

    pub fn fingerprint(&self) -> String {
        hex(&self.fingerprint[..8])
    }

    #[cfg(test)]
    pub fn subkey(&self, purpose: &str) -> [u8; 32] {
        match purpose {
            "name-enc" => self.name_enc,
            "name-siv" => self.name_siv,
            "chunk-enc" => self.chunk_enc,
            "chunk-siv" => self.chunk_siv,
            "file-hash" => self.file_hash,
            "auth" => self.auth,
            "fingerprint" => self.fingerprint,
            _ => panic!("unknown purpose {purpose}"),
        }
    }

    /* ---- names ---- */

    pub fn encrypt_name(&self, name: &str) -> Result<String, E2eeErr> {
        let raw = name.as_bytes();
        if raw.is_empty() || raw.len() > MAX_NAME || name.contains('/') {
            return Err(E2eeErr::Format(format!("{name:?}: a name is 1–255 bytes without /")));
        }
        let mut padded = raw.to_vec();
        padded.push(0x80);
        while padded.len() % NAME_PAD != 0 {
            padded.push(0);
        }
        Ok(format!("~{}", URL_SAFE_NO_PAD.encode(siv_seal(&self.name_enc, &self.name_siv, AD_NAME, &padded))))
    }

    pub fn decrypt_name(&self, seg: &str) -> Result<String, E2eeErr> {
        let b64 = seg.strip_prefix('~').ok_or(E2eeErr::Format("not an encrypted name".into()))?;
        let blob = URL_SAFE_NO_PAD.decode(b64).map_err(|_| E2eeErr::Format("not base64url".into()))?;
        let padded = siv_open(&self.name_enc, &self.name_siv, AD_NAME, &blob)?;
        let end = padded.iter().rposition(|&b| b != 0).ok_or(E2eeErr::Format("bad padding".into()))?;
        if padded[end] != 0x80 || end == 0 {
            return Err(E2eeErr::Format("bad padding".into()));
        }
        let s = String::from_utf8(padded[..end].to_vec()).map_err(|_| E2eeErr::Format("not UTF-8".into()))?;
        if s.contains('/') {
            return Err(E2eeErr::Format("a name can't contain /".into()));
        }
        Ok(s)
    }

    /// Each `/`-separated segment on its own.
    pub fn encrypt_path(&self, path: &str) -> Result<String, E2eeErr> {
        Ok(path.split('/').map(|s| self.encrypt_name(s)).collect::<Result<Vec<_>, _>>()?.join("/"))
    }

    pub fn decrypt_path(&self, path: &str) -> Result<String, E2eeErr> {
        Ok(path.split('/').map(|s| self.decrypt_name(s)).collect::<Result<Vec<_>, _>>()?.join("/"))
    }

    /* ---- content ---- */

    pub fn seal_chunk(&self, piece: &[u8]) -> Vec<u8> {
        let mut out = vec![CHUNK_VERSION];
        out.extend(siv_seal(&self.chunk_enc, &self.chunk_siv, AD_CHUNK, piece));
        out
    }

    pub fn open_chunk(&self, sealed: &[u8]) -> Result<Vec<u8>, E2eeErr> {
        match sealed.first() {
            Some(&CHUNK_VERSION) => siv_open(&self.chunk_enc, &self.chunk_siv, AD_CHUNK, &sealed[1..]),
            Some(v) => Err(E2eeErr::Format(format!("unknown chunk version {v}"))),
            None => Err(E2eeErr::Format("empty chunk".into())),
        }
    }

    /// Drive's `sha256` for a file on an encrypted account, from the
    /// plaintext's SHA-256 (hex). Equal tags mean equal content.
    pub fn tag_sha(&self, plain_sha_hex: &str) -> String {
        match unhex(plain_sha_hex) {
            Some(d) => hex(&hmac256(&self.file_hash, &[&d])),
            None => String::new(),
        }
    }

    pub fn file_hash(&self, plaintext: &[u8]) -> String {
        hex(&hmac256(&self.file_hash, &[&Sha256::digest(plaintext)]))
    }

    /// Split, seal and describe content for a commit. An empty file has no chunks.
    pub fn seal_content(&self, data: &[u8]) -> Vec<(String, Vec<u8>)> {
        data.chunks(PIECE).map(|p| {
            let s = self.seal_chunk(p);
            (hex(&Sha256::digest(&s)), s)
        }).collect()
    }
}

/* ======================= the bundle ======================= */

#[derive(serde::Deserialize, serde::Serialize, Clone, Debug, PartialEq)]
pub struct Kdf {
    pub alg: String,
    pub time: u32,
    pub memory_kib: u32,
    pub threads: u32,
    pub salt: String,
}

#[derive(serde::Deserialize, serde::Serialize, Clone, Debug, PartialEq)]
pub struct Bundle {
    pub version: u32,
    pub kdf: Kdf,
    pub passphrase_wrapped: String,
    pub recovery_wrapped: String,
    pub recovery_salt: String,
    pub fingerprint: String,
}

fn b64(s: &str, what: &str, len: usize) -> Result<Vec<u8>, E2eeErr> {
    let b = STANDARD.decode(s).map_err(|_| E2eeErr::Format(format!("{what} isn't base64")))?;
    if b.len() != len {
        return Err(E2eeErr::Format(format!("{what} should be {len} bytes")));
    }
    Ok(b)
}

impl Bundle {
    /// Refuse what we don't understand, before deriving anything (§1.2).
    pub fn check(&self) -> Result<(), E2eeErr> {
        let bad = |m: &str| Err(E2eeErr::Format(format!("Null Drive's key bundle uses settings this version of Null Notes doesn't support ({m}).")));
        if self.version != 1 {
            return bad("version");
        }
        if self.kdf.alg != "argon2id" {
            return bad("kdf");
        }
        if !(1..=20).contains(&self.kdf.time) || !(8192..=1_048_576).contains(&self.kdf.memory_kib) || self.kdf.threads != 1 {
            return bad("kdf parameters");
        }
        b64(&self.kdf.salt, "salt", 16)?;
        b64(&self.recovery_salt, "recovery salt", 16)?;
        b64(&self.passphrase_wrapped, "passphrase wrap", NONCE + KEY + TAG)?;
        b64(&self.recovery_wrapped, "recovery wrap", NONCE + KEY + TAG)?;
        Ok(())
    }
}

fn unwrap_master(kek: &[u8], blob: &[u8], ad: &[u8]) -> Result<Vec<u8>, E2eeErr> {
    aead_open(kek, &blob[..NONCE], &blob[NONCE..], ad)
}

/// Crockford base32, 32 characters → 20 bytes. Lenient: case, `-`, spaces,
/// O→0, I/L→1.
pub fn parse_recovery_key(s: &str) -> Result<Vec<u8>, E2eeErr> {
    let (mut acc, mut bits, mut n) = (0u32, 0u32, 0usize);
    let mut out = Vec::with_capacity(20);
    for ch in s.chars() {
        let c = match ch.to_ascii_uppercase() {
            '-' | ' ' | '\t' | '\r' | '\n' => continue,
            'O' => '0',
            'I' | 'L' => '1',
            c => c,
        };
        let i = CROCKFORD.iter().position(|&x| x as char == c).ok_or(E2eeErr::Format("A recovery key only has letters and digits, like B9D5-MPJT-….".into()))?;
        acc = (acc << 5) | i as u32;
        bits += 5;
        n += 1;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
        acc &= (1 << bits) - 1;
    }
    if n != 32 || out.len() != 20 {
        return Err(E2eeErr::Format("A recovery key is 32 characters, like B9D5-MPJT-B9D5-MPJT-B9D5-MPJT-B9D5-MPJT.".into()));
    }
    Ok(out)
}

#[cfg(test)]
pub fn format_recovery_key(b: &[u8]) -> String {
    let mut bitstr = String::new();
    for x in b {
        bitstr.push_str(&format!("{x:08b}"));
    }
    let chars: String = (0..32).map(|i| CROCKFORD[usize::from_str_radix(&bitstr[i * 5..i * 5 + 5], 2).unwrap()] as char).collect();
    (0..8).map(|i| &chars[i * 4..i * 4 + 4]).collect::<Vec<_>>().join("-")
}

/// What the user typed: the E2EE passphrase, or the recovery key.
pub enum Secret<'a> {
    Passphrase(&'a str),
    RecoveryKey(&'a str),
}

/// Looks like a recovery key: 32 Crockford characters once dashes and
/// spaces are gone. Anything else is treated as the passphrase.
pub fn guess_secret(s: &str) -> Secret<'_> {
    let compact: String = s.chars().filter(|c| !matches!(c, '-' | ' ')).collect();
    if compact.len() == 32 && s.contains('-') && parse_recovery_key(s).is_ok() {
        Secret::RecoveryKey(s)
    } else {
        Secret::Passphrase(s)
    }
}

/// Unlock the account key and check its fingerprint against the bundle.
pub fn unlock(bundle: &Bundle, secret: &Secret) -> Result<Keys, E2eeErr> {
    bundle.check()?;
    let mut master = match secret {
        Secret::Passphrase(p) => {
            let params = argon2::Params::new(bundle.kdf.memory_kib, bundle.kdf.time, 1, Some(KEY)).map_err(|e| E2eeErr::Format(e.to_string()))?;
            let mut kek = [0u8; KEY];
            argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params)
                .hash_password_into(p.as_bytes(), &b64(&bundle.kdf.salt, "salt", 16)?, &mut kek)
                .map_err(|e| E2eeErr::Format(e.to_string()))?;
            let m = unwrap_master(&kek, &b64(&bundle.passphrase_wrapped, "passphrase wrap", 72)?, AD_WRAP_PASS);
            kek.zeroize();
            m?
        }
        Secret::RecoveryKey(r) => {
            let mut rk = parse_recovery_key(r)?;
            let mut kek = hkdf(&rk, Some(&b64(&bundle.recovery_salt, "recovery salt", 16)?), b"nulldrive/v1/recovery-kek");
            let m = unwrap_master(&kek, &b64(&bundle.recovery_wrapped, "recovery wrap", 72)?, AD_WRAP_RECOVERY);
            rk.zeroize();
            kek.zeroize();
            m?
        }
    };
    let keys = Keys::from_master(&master);
    master.zeroize();
    let keys = keys?;
    if keys.fingerprint() != bundle.fingerprint {
        return Err(E2eeErr::Fingerprint);
    }
    Ok(keys)
}

/* ======================= Windows-safe names ======================= */

/// Drive can't check names it can't read, so Notes applies the rules from
/// notes-backup-drive-api.md itself (a desktop client pulling the file
/// would trip over them). `Some(reason)` = don't upload.
pub fn unsafe_name(path: &str) -> Option<String> {
    for seg in path.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." {
            return Some("empty or relative path segment".into());
        }
        if seg.len() > MAX_NAME {
            return Some("a name is longer than 255 bytes".into());
        }
        if let Some(c) = seg.chars().find(|c| matches!(c, '<' | '>' | ':' | '"' | '\\' | '|' | '?' | '*') || (*c as u32) < 0x20) {
            return Some(format!("the name contains {:?}", c));
        }
        if seg.ends_with('.') || seg.ends_with(' ') {
            return Some("the name ends with a dot or space".into());
        }
        let stem = seg.split('.').next().unwrap_or("").trim_end().to_ascii_uppercase();
        let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || ((stem.starts_with("COM") || stem.starts_with("LPT")) && stem.len() == 4 && stem.as_bytes()[3].is_ascii_digit() && stem.as_bytes()[3] != b'0');
        if reserved {
            return Some(format!("{stem} is a reserved name on Windows"));
        }
    }
    None
}

/* ======================= tests: Drive's vectors ======================= */

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn vectors() -> Value {
        serde_json::from_str(include_str!("../testdata/e2ee/vectors.json")).unwrap()
    }
    fn notes_vector() -> Value {
        serde_json::from_str(include_str!("../testdata/e2ee/notes-vector.json")).unwrap()
    }
    fn s(v: &Value) -> &str {
        v.as_str().unwrap()
    }

    #[test]
    fn vectors_json_keys_names_paths_chunks_hashes() {
        let v = vectors();
        let k = Keys::from_master(&unhex(s(&v["master"])).unwrap()).unwrap();
        assert_eq!(k.fingerprint(), s(&v["fingerprint"]));
        assert_eq!(hex(&k.subkey("auth")), s(&v["auth_key"]));
        for n in v["names"].as_array().unwrap() {
            assert_eq!(k.encrypt_name(s(&n["Plain"])).unwrap(), s(&n["Enc"]), "name {:?}", s(&n["Plain"]));
            assert_eq!(k.decrypt_name(s(&n["Enc"])).unwrap(), s(&n["Plain"]));
        }
        for p in v["paths"].as_array().unwrap() {
            assert_eq!(k.encrypt_path(s(&p["Plain"])).unwrap(), s(&p["Enc"]));
            assert_eq!(k.decrypt_path(s(&p["Enc"])).unwrap(), s(&p["Plain"]));
        }
        for c in v["chunks"].as_array().unwrap() {
            let pt = unhex(s(&c["Plain"])).unwrap();
            assert_eq!(hex(&k.seal_chunk(&pt)), s(&c["Sealed"]), "chunk of {} bytes", pt.len());
            assert_eq!(k.open_chunk(&unhex(s(&c["Sealed"])).unwrap()).unwrap(), pt);
        }
        for f in v["file_hashes"].as_array().unwrap() {
            assert_eq!(k.tag_sha(s(&f["SHA256"])), s(&f["Mapped"]));
        }
        assert_eq!(k.file_hash(b""), s(&v["file_hashes"][0]["Mapped"]), "the empty file");
        assert_eq!(k.file_hash(b"hello"), s(&v["file_hashes"][1]["Mapped"]));
    }

    #[test]
    fn vectors_json_bundle_unlocks_by_passphrase_and_recovery_key() {
        let v = vectors();
        let b: Bundle = serde_json::from_value(v["bundle"].clone()).unwrap();
        let k = unlock(&b, &Secret::Passphrase(s(&v["passphrase"]))).unwrap();
        assert_eq!(k.master_hex(), s(&v["master"]));
        let k = unlock(&b, &Secret::RecoveryKey(s(&v["recovery_key"]))).unwrap();
        assert_eq!(k.master_hex(), s(&v["master"]));
        // lenient recovery key parsing: case, spaces, O/I/L
        let messy = s(&v["recovery_key"]).to_lowercase().replace('-', " ");
        assert_eq!(unlock(&b, &Secret::RecoveryKey(&messy)).unwrap().master_hex(), s(&v["master"]));
        assert_eq!(format_recovery_key(&parse_recovery_key(s(&v["recovery_key"])).unwrap()), s(&v["recovery_key"]));
        assert_eq!(parse_recovery_key("0000-0000-0000-0000-0000-0000-0000-0000").unwrap(), parse_recovery_key("OOOO-oooo-0000-0000-0000-0000-0000-0000").unwrap());
        // what the user typed decides which unwrap is tried
        assert!(matches!(guess_secret(s(&v["recovery_key"])), Secret::RecoveryKey(_)));
        assert!(matches!(guess_secret(s(&v["passphrase"])), Secret::Passphrase(_)));
        // wrong secrets are refused, never a wrong key
        assert_eq!(unlock(&b, &Secret::Passphrase("wrong passphrase")).unwrap_err(), E2eeErr::Decrypt);
        assert_eq!(unlock(&b, &Secret::RecoveryKey("ZZZZ-ZZZZ-ZZZZ-ZZZZ-ZZZZ-ZZZZ-ZZZZ-ZZZZ")).unwrap_err(), E2eeErr::Decrypt);
        assert!(parse_recovery_key("B9D5-MPJT").is_err());
        // a bundle whose fingerprint isn't the key's is refused
        let mut lie = b.clone();
        lie.fingerprint = "0000000000000000".into();
        assert_eq!(unlock(&lie, &Secret::Passphrase(s(&v["passphrase"]))).unwrap_err(), E2eeErr::Fingerprint);
    }

    #[test]
    fn bundles_we_dont_understand_are_refused_before_deriving() {
        let b: Bundle = serde_json::from_value(vectors()["bundle"].clone()).unwrap();
        let bad = |f: &dyn Fn(&mut Bundle)| {
            let mut x = b.clone();
            f(&mut x);
            assert!(matches!(x.check(), Err(E2eeErr::Format(_))), "{x:?}");
        };
        bad(&|x| x.version = 2);
        bad(&|x| x.kdf.alg = "scrypt".into());
        bad(&|x| x.kdf.time = 0);
        bad(&|x| x.kdf.time = 21);
        bad(&|x| x.kdf.memory_kib = 4096);
        bad(&|x| x.kdf.memory_kib = 2_000_000);
        bad(&|x| x.kdf.threads = 4);
        bad(&|x| x.kdf.salt = STANDARD.encode([0u8; 8]));
        bad(&|x| x.passphrase_wrapped = STANDARD.encode([0u8; 40]));
        assert!(b.check().is_ok());
    }

    #[test]
    fn notes_vector_worked_example() {
        let n = notes_vector();
        let k = Keys::from_master(&unhex(s(&n["master"])).unwrap()).unwrap();
        for p in PURPOSES {
            assert_eq!(hex(&k.subkey(p)), s(&n["subkeys"][p]), "subkey {p}");
        }
        assert_eq!(k.fingerprint(), s(&n["fingerprint"]));
        assert_eq!(hex(&Sha256::digest(hex(&k.subkey("auth")).as_bytes())), s(&n["auth_hash"]));
        for seg in n["segments"].as_array().unwrap() {
            assert_eq!(k.encrypt_name(s(&seg["plain"])).unwrap(), s(&seg["enc"]));
            // the synthetic nonce is the first 24 bytes of the sealed name
            let blob = URL_SAFE_NO_PAD.decode(&s(&seg["enc"])[1..]).unwrap();
            assert_eq!(hex(&blob[..24]), s(&seg["nonce"]));
        }
        assert_eq!(k.encrypt_path(s(&n["path"])).unwrap(), s(&n["encrypted_path"]));
        assert_eq!(k.decrypt_path(s(&n["encrypted_path"])).unwrap(), s(&n["path"]));

        let content = s(&n["content"]).as_bytes();
        assert_eq!(content.len(), 81);
        assert_eq!(hex(&Sha256::digest(content)), s(&n["content_sha256"]));
        assert_eq!(k.file_hash(content), s(&n["file_hash"]));
        assert_eq!(k.tag_sha(s(&n["content_sha256"])), s(&n["file_hash"]));
        let chunks = k.seal_content(content);
        assert_eq!(chunks.len(), 1);
        assert_eq!(hex(&chunks[0].1), s(&n["sealed_chunk"]));
        assert_eq!(hex(&chunks[0].1[1..25]), s(&n["chunk_nonce"]));
        assert_eq!(chunks[0].0, s(&n["chunk_hash"]));
        // the commit body Notes sends (crate::backup builds it the same way)
        let commit = crate::backup::commit_body(&k, content, &chunks, Some("2026-10-18T08:00:00Z"));
        assert_eq!(commit, n["commit"]);
    }

    #[test]
    fn tampering_and_edge_cases() {
        let k = Keys::from_master(&[7u8; 32]).unwrap();
        let other = Keys::from_master(&[8u8; 32]).unwrap();
        let e = k.encrypt_name("Plan.md").unwrap();
        assert_eq!(e.len(), 76);
        assert!(other.decrypt_name(&e).is_err(), "another key can't read it");
        assert!(k.decrypt_name("Plan.md").is_err(), "plaintext leftovers don't decrypt");
        let mut t = e.clone().into_bytes();
        let i = t.len() - 5;
        t[i] = if t[i] == b'A' { b'B' } else { b'A' };
        assert!(k.decrypt_name(std::str::from_utf8(&t).unwrap()).is_err(), "tampered name");
        let max = "n".repeat(255);
        assert_eq!(k.encrypt_name(&max).unwrap().len(), 396);
        assert!(k.encrypt_name(&"n".repeat(256)).is_err());
        assert!(k.encrypt_name("").is_err());
        assert!(k.encrypt_path("a//b").is_err());
        let sealed = k.seal_chunk(b"hello");
        let mut bad = sealed.clone();
        bad[0] = 2;
        assert!(k.open_chunk(&bad).is_err(), "unknown chunk version");
        let mut bad = sealed.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(k.open_chunk(&bad).is_err(), "tampered chunk");
        assert!(k.seal_content(b"").is_empty(), "an empty file has no chunks");
        let big = vec![1u8; PIECE * 2 + 3];
        let parts = k.seal_content(&big);
        assert_eq!(parts.len(), 3);
        assert!(parts.iter().all(|(_, s)| s.len() <= MAX_SEALED));
        assert_eq!(parts.iter().map(|(_, s)| s.len() - CHUNK_OVERHEAD).sum::<usize>(), big.len());
    }

    #[test]
    fn windows_unsafe_names() {
        for bad in ["CON.md", "a/aux.txt", "lpt1", "Com9.md", "x:y.md", "what?.md", "dot.", "space ", "a/<b>.md", "tab\there"] {
            assert!(unsafe_name(bad).is_some(), "{bad}");
        }
        for ok in ["Null Notes/Work/Plan.md", "COM0.md", "console.md", "CONTRA.md", "Ünïcödé 📁/a.md", ".nullnotes/ids.json"] {
            assert!(unsafe_name(ok).is_none(), "{ok}");
        }
    }
}
