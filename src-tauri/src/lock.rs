//! Vault lock: a passphrase protects one random 32-byte data key, and the data
//! key encrypts each locked note. Format recovered from the 0.3.1 build, so
//! vaults locked by the shipped app still open:
//!
//! `.nullnotes/lock.json`
//! ```json
//! { "version": 1, "kdf": "argon2id", "m_kib": 65536, "t": 3, "p": 1,
//!   "salt": "<base64, 16 bytes>", "wrapped": "<base64, nonce|ct|tag>" }
//! ```
//! - KEK = Argon2id v0x13(passphrase, salt, m_kib, t, p) -> 32 bytes
//! - wrapped = XChaCha20-Poly1305(KEK, data key, aad = "null-notes data key v1")
//!
//! `<note>.lock` = `NNLOCK1\n` | nonce(24) | ciphertext | tag(16),
//! encrypted with the data key, aad = `NNLOCK1\n`.

use base64::{engine::general_purpose, Engine};
use chacha20poly1305::aead::{rand_core::RngCore, Aead, AeadCore, KeyInit, OsRng, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

pub const FILE_MAGIC: &[u8; 8] = b"NNLOCK1\n";
const WRAP_AAD: &[u8] = b"null-notes data key v1";
const NONCE_LEN: usize = 24;

pub type Key = Zeroizing<[u8; 32]>;

#[derive(Serialize, Deserialize)]
pub struct LockFile {
    pub version: u32,
    pub kdf: String,
    pub m_kib: u32,
    pub t: u32,
    pub p: u32,
    pub salt: String,
    pub wrapped: String,
}

fn b64d(s: &str) -> Option<Vec<u8>> {
    general_purpose::STANDARD
        .decode(s.trim())
        .or_else(|_| general_purpose::STANDARD_NO_PAD.decode(s.trim()))
        .ok()
}

fn derive(pass: &str, salt: &[u8], m_kib: u32, t: u32, p: u32) -> Result<Key, String> {
    let params = argon2::Params::new(m_kib, t, p, Some(32)).map_err(|e| format!("Bad key settings: {e}"))?;
    let a = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let mut out = Zeroizing::new([0u8; 32]);
    a.hash_password_into(pass.as_bytes(), salt, out.as_mut())
        .map_err(|e| format!("Couldn't derive the key: {e}"))?;
    Ok(out)
}

/// nonce | ciphertext | tag
fn seal(key: &[u8; 32], plain: &[u8], aad: &[u8]) -> Result<Vec<u8>, String> {
    let c = XChaCha20Poly1305::new(key.into());
    let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
    let ct = c.encrypt(&nonce, Payload { msg: plain, aad }).map_err(|_| "Encryption failed.".to_string())?;
    let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

fn open(key: &[u8; 32], blob: &[u8], aad: &[u8]) -> Option<Vec<u8>> {
    if blob.len() < NONCE_LEN + 16 {
        return None;
    }
    let c = XChaCha20Poly1305::new(key.into());
    c.decrypt(XNonce::from_slice(&blob[..NONCE_LEN]), Payload { msg: &blob[NONCE_LEN..], aad }).ok()
}

/// New lock: random data key wrapped under the passphrase.
pub fn setup(pass: &str) -> Result<(LockFile, Key), String> {
    let mut key = Zeroizing::new([0u8; 32]);
    OsRng.fill_bytes(key.as_mut());
    let lf = wrap(pass, &key)?;
    Ok((lf, key))
}

/// Wrap an existing data key under a (new) passphrase with a fresh salt.
pub fn wrap(pass: &str, key: &Key) -> Result<LockFile, String> {
    let (m_kib, t, p) = (65536, 3, 1);
    let mut salt = [0u8; 16];
    OsRng.fill_bytes(&mut salt);
    let kek = derive(pass, &salt, m_kib, t, p)?;
    let wrapped = seal(&kek, key.as_ref(), WRAP_AAD)?;
    Ok(LockFile {
        version: 1,
        kdf: "argon2id".into(),
        m_kib,
        t,
        p,
        salt: general_purpose::STANDARD.encode(salt),
        wrapped: general_purpose::STANDARD.encode(wrapped),
    })
}

pub fn unlock(lf: &LockFile, pass: &str) -> Result<Key, String> {
    if lf.kdf != "argon2id" {
        return Err("The vault's lock file is damaged.".into());
    }
    let salt = b64d(&lf.salt).ok_or("The vault's lock file is damaged.")?;
    let wrapped = b64d(&lf.wrapped).ok_or("The vault's lock file is damaged.")?;
    let kek = derive(pass, &salt, lf.m_kib, lf.t, lf.p)?;
    let raw = Zeroizing::new(open(&kek, &wrapped, WRAP_AAD).ok_or("Wrong passphrase.")?);
    if raw.len() != 32 {
        return Err("The vault's lock file is damaged.".into());
    }
    let mut key = Zeroizing::new([0u8; 32]);
    key.copy_from_slice(&raw);
    Ok(key)
}

pub fn encrypt_file(key: &Key, plain: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = FILE_MAGIC.to_vec();
    out.extend(seal(key, plain, FILE_MAGIC)?);
    Ok(out)
}

pub fn decrypt_file(key: &Key, data: &[u8], name: &str) -> Result<Vec<u8>, String> {
    if !data.starts_with(FILE_MAGIC) {
        return Err(format!("{name} isn't a Null Notes locked file."));
    }
    open(key, &data[FILE_MAGIC.len()..], FILE_MAGIC)
        .ok_or_else(|| format!("{name} couldn't be unlocked. It may be damaged or from another vault."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_wrong_pass() {
        let (lf, key) = setup("correct horse").unwrap();
        let json = serde_json::to_string(&lf).unwrap();
        let lf: LockFile = serde_json::from_str(&json).unwrap();
        let k2 = unlock(&lf, "correct horse").unwrap();
        assert_eq!(*key, *k2);
        assert_eq!(unlock(&lf, "nope nope").unwrap_err(), "Wrong passphrase.");
        let enc = encrypt_file(&key, b"# secret").unwrap();
        assert!(enc.starts_with(b"NNLOCK1\n"));
        assert_eq!(enc.len(), 8 + 24 + 8 + 16);
        assert_eq!(decrypt_file(&k2, &enc, "x").unwrap(), b"# secret");
        // re-wrap keeps the same data key, so locked files stay readable
        let lf3 = wrap("new passphrase", &key).unwrap();
        assert_eq!(*unlock(&lf3, "new passphrase").unwrap(), *key);
    }
}
