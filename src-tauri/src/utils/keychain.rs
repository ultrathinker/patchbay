//! macOS secret store: the counterpart of Windows DPAPI.
//!
//! One random 256-bit key is kept as a generic password in the user's login
//! Keychain (service `dev.patchbay.app`, account `config-encryption-key`). Each
//! secret config value is sealed with AES-256-GCM under that key and stored
//! inline in `patchbay.json` as `keychain:<base64(nonce || ciphertext+tag)>`.
//!
//! Why a key in the Keychain rather than one Keychain item per secret: the JSON
//! stays the single source of truth (jacks are added, renamed and deleted by
//! editing it, with no Keychain items to orphan), and macOS asks at most once
//! per app signature for access to one item instead of once per secret.
//!
//! Like DPAPI, only the same macOS user can open the values: the key never
//! leaves the Keychain except into this process's memory, and a copied
//! `patchbay.json` (a backup, another Mac) is useless without it.
//!
//! The key is created only when the Keychain says the item does not exist.
//! Any other failure (the user denied access, the Keychain is locked) is an
//! error, never a reason to generate a new key: that would make every value
//! already sealed with the old key unreadable.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use parking_lot::Mutex;
use security_framework::passwords::{get_generic_password, set_generic_password};
use security_framework::random::SecRandom;

use crate::utils::log::log;

/// Prefix of values sealed by this module.
pub const PREFIX: &str = "keychain:";

const SERVICE: &str = "dev.patchbay.app";
const ACCOUNT: &str = "config-encryption-key";
/// `errSecItemNotFound`.
const ERR_ITEM_NOT_FOUND: i32 = -25300;
const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;

/// The key, read from the Keychain once per run.
static KEY: Mutex<Option<[u8; KEY_LEN]>> = Mutex::new(None);

fn random_bytes(buf: &mut [u8]) -> Result<(), String> {
    SecRandom::default()
        .copy_bytes(buf)
        .map_err(|e| format!("SecRandom failed: {e}"))
}

/// The encryption key: cached, else read from the Keychain, else (only when the
/// item does not exist yet) generated and stored.
fn key() -> Result<[u8; KEY_LEN], String> {
    let mut cached = KEY.lock();
    if let Some(k) = *cached {
        return Ok(k);
    }
    let key = match get_generic_password(SERVICE, ACCOUNT) {
        Ok(bytes) => {
            let k: [u8; KEY_LEN] = bytes.as_slice().try_into().map_err(|_| {
                format!(
                    "the Keychain item {SERVICE}/{ACCOUNT} has {} bytes, expected {KEY_LEN}",
                    bytes.len()
                )
            })?;
            k
        }
        Err(e) if e.code() == ERR_ITEM_NOT_FOUND => {
            let mut k = [0u8; KEY_LEN];
            random_bytes(&mut k)?;
            set_generic_password(SERVICE, ACCOUNT, &k)
                .map_err(|e| format!("storing the key in the Keychain failed: {e}"))?;
            log("keychain: created the config encryption key in the login Keychain");
            k
        }
        Err(e) => return Err(format!("reading the key from the Keychain failed: {e}")),
    };
    *cached = Some(key);
    Ok(key)
}

/// Seal `plaintext` under `key` with the given nonce. Returns
/// `nonce || ciphertext+tag`. Pure, so it is unit-tested without a Keychain.
fn seal(key: &[u8; KEY_LEN], nonce: &[u8; NONCE_LEN], plaintext: &[u8]) -> Result<Vec<u8>, String> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let ct = cipher
        .encrypt(Nonce::from_slice(nonce), plaintext)
        .map_err(|_| "AES-GCM encryption failed".to_string())?;
    let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
    out.extend_from_slice(nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Open a `nonce || ciphertext+tag` blob sealed by [`seal`].
fn open(key: &[u8; KEY_LEN], blob: &[u8]) -> Result<Vec<u8>, String> {
    if blob.len() < NONCE_LEN + 16 {
        return Err("sealed value is too short".to_string());
    }
    let (nonce, ct) = blob.split_at(NONCE_LEN);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    cipher
        .decrypt(Nonce::from_slice(nonce), ct)
        .map_err(|_| "AES-GCM decryption failed (wrong key or damaged value)".to_string())
}

/// Encrypt a non-empty field value: `keychain:<base64>`.
pub fn encrypt_field(value: &str) -> Result<String, String> {
    let result = (|| {
        let key = key()?;
        let mut nonce = [0u8; NONCE_LEN];
        random_bytes(&mut nonce)?;
        let blob = seal(&key, &nonce, value.as_bytes())?;
        Ok(format!("{PREFIX}{}", BASE64.encode(blob)))
    })();
    if let Err(e) = &result {
        log(&format!("keychain: encrypt_field failed: {e}"));
    }
    result
}

/// Decrypt a `keychain:` field value. Any failure logs and returns an empty
/// string (the user re-enters the secret), as the DPAPI path does.
pub fn decrypt_field(value: &str) -> String {
    let Some(b64) = value.strip_prefix(PREFIX) else {
        return value.to_string();
    };
    let result = (|| {
        let blob = BASE64.decode(b64).map_err(|e| format!("base64 decode: {e}"))?;
        let key = key()?;
        let plain = open(&key, &blob)?;
        String::from_utf8(plain).map_err(|e| format!("UTF-8 decode: {e}"))
    })();
    match result {
        Ok(s) => s,
        Err(e) => {
            log(&format!("keychain: decrypt_field failed: {e}"));
            String::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const K: [u8; KEY_LEN] = [7u8; KEY_LEN];
    const N: [u8; NONCE_LEN] = [3u8; NONCE_LEN];

    #[test]
    fn seal_then_open_round_trips() {
        let blob = seal(&K, &N, b"ghp_secret").unwrap();
        assert_eq!(&blob[..NONCE_LEN], &N);
        assert_eq!(open(&K, &blob).unwrap(), b"ghp_secret");
    }

    #[test]
    fn a_wrong_key_or_a_damaged_value_does_not_open() {
        let mut blob = seal(&K, &N, b"ghp_secret").unwrap();
        assert!(open(&[8u8; KEY_LEN], &blob).is_err());
        let last = blob.len() - 1;
        blob[last] ^= 1;
        assert!(open(&K, &blob).is_err());
        assert!(open(&K, &[0u8; 5]).is_err());
    }

    /// Talks to the real login Keychain, so it needs a logged-in desktop
    /// session (not a plain SSH shell): `cargo test -- --ignored keychain`.
    #[test]
    #[ignore]
    fn keychain_round_trip_through_the_login_keychain() {
        let enc = encrypt_field("live-secret").expect("encrypt");
        assert!(enc.starts_with(PREFIX));
        assert_ne!(enc, encrypt_field("live-secret").unwrap(), "fresh nonce each time");
        assert_eq!(decrypt_field(&enc), "live-secret");
    }
}
