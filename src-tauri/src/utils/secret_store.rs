//! Platform-neutral encryption of secret config values at rest.
//!
//! Each OS has its own user-bound secret store, and each marks the values it
//! encrypted with its own prefix so a value always says how to open it:
//!
//! | OS      | Prefix      | How                                                     |
//! |---------|-------------|---------------------------------------------------------|
//! | Windows | `dpapi:`    | DPAPI, CurrentUser scope ([`crate::utils::dpapi`])      |
//! | macOS   | `keychain:` | AES-256-GCM, key in the login Keychain ([`crate::utils::keychain`]) |
//!
//! A value with no known prefix is plaintext (a hand-edited config) and is
//! passed through unchanged; it gets encrypted on the next save. A value
//! encrypted on the OTHER OS (a config copied between machines) cannot be
//! opened here: it decrypts to an empty string and the log says to re-enter it,
//! the same outcome as a corrupted blob.

/// Every prefix any build may have written. Used to recognise an
/// already-encrypted value (never encrypt twice), whichever OS wrote it.
pub const ENCRYPTED_PREFIXES: &[&str] = &["dpapi:", "keychain:"];

/// True when `value` is already encrypted by any platform's store.
pub fn is_encrypted(value: &str) -> bool {
    ENCRYPTED_PREFIXES.iter().any(|p| value.starts_with(p))
}

/// Encrypt a field value for inline storage in JSON, with this platform's
/// prefix. An empty value stays empty. On failure the caller keeps the
/// plaintext (the user's data is never silently dropped); the reason is logged.
pub fn encrypt_field(value: &str) -> Result<String, String> {
    if value.is_empty() {
        return Ok(String::new());
    }
    #[cfg(windows)]
    {
        crate::utils::dpapi::encrypt_field(value)
    }
    #[cfg(target_os = "macos")]
    {
        crate::utils::keychain::encrypt_field(value)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        Err("no secret store on this platform; value kept as plaintext".to_string())
    }
}

/// Decrypt a field value from JSON. Plaintext passes through; a value from
/// this platform's store is opened; a value from another platform's store
/// (or a corrupted one) becomes an empty string, with a log line.
pub fn decrypt_field(value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    #[cfg(windows)]
    if value.starts_with("dpapi:") {
        return crate::utils::dpapi::decrypt_field(value);
    }
    #[cfg(target_os = "macos")]
    if value.starts_with(crate::utils::keychain::PREFIX) {
        return crate::utils::keychain::decrypt_field(value);
    }
    if is_encrypted(value) {
        let prefix = value.split(':').next().unwrap_or("");
        crate::utils::log::log(&format!(
            "secrets: a '{prefix}:' value was encrypted on another operating system and \
             cannot be opened here; re-enter it in patchbay.json"
        ));
        return String::new();
    }
    value.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_every_platform_prefix() {
        assert!(is_encrypted("dpapi:AAAA"));
        assert!(is_encrypted("keychain:AAAA"));
        assert!(!is_encrypted("plain-secret"));
        assert!(!is_encrypted(""));
    }

    #[test]
    fn plaintext_and_empty_pass_through() {
        assert_eq!(decrypt_field(""), "");
        assert_eq!(decrypt_field("hunter2"), "hunter2");
        assert_eq!(encrypt_field("").unwrap(), "");
    }

    #[test]
    fn a_foreign_platform_value_opens_to_empty() {
        #[cfg(windows)]
        let foreign = "keychain:AAAA";
        #[cfg(not(windows))]
        let foreign = "dpapi:AAAA";
        assert_eq!(decrypt_field(foreign), "");
    }
}
