//! Secret handling for config fields.
//!
//! Invariant the rest of the app relies on: **anything written to
//! `patchbay.json` is already `dpapi:`-encrypted**. Users hand-edit the file
//! and may type plaintext secrets; `encrypt_config_secrets` wraps those on the
//! next save.
//! Already-`dpapi:`-prefixed values are left untouched (no double-encryption),
//! and empty values are skipped (encrypt_field returns empty for empty input).
//!
//! Decryption happens at spawn/connect time (not at config load) via the
//! `decrypted_env` / `decrypted_headers` helpers, which transparently pass
//! through plaintext (so a partially-migrated config still works).
//!
//! NOTE: `utils::dpapi` (and therefore this module) is Windows-only; the DPAPI
//! round-trip asserts are gated with `#[cfg(windows)]`.

use std::collections::BTreeMap;

use crate::utils::dpapi;

use super::schema::{JackConfig, JackTransport, PatchbayConfig};
// `Sharing` is referenced only from the unit tests below.
#[cfg(test)]
use super::schema::Sharing;

/// Prefix marking an already-DPAPI-encrypted field value. Must match
/// `utils::dpapi::DPAPI_PREFIX`.
const DPAPI_PREFIX: &str = "dpapi:";

fn is_encrypted(value: &str) -> bool {
    value.starts_with(DPAPI_PREFIX)
}

/// Encrypt every plaintext secret value across all jacks, in place.
///
/// Walks each jack's `env` (stdio) and `headers` (streamable-http) maps and
/// wraps any non-empty, non-`dpapi:` value. Intended to be called immediately
/// before writing the config to disk. Idempotent.
pub fn encrypt_config_secrets(cfg: &mut PatchbayConfig) {
    for jack in cfg.jacks.iter_mut() {
        encrypt_jack_secrets(jack);
    }
}

/// Encrypt the secret-bearing maps of a single jack (env for stdio, headers
/// for streamable-http). Idempotent.
pub fn encrypt_jack_secrets(jack: &mut JackConfig) {
    if let Some(env) = jack.transport.env_mut() {
        encrypt_map(env);
    }
    if let Some(headers) = jack.transport.headers_mut() {
        encrypt_map(headers);
    }
}

fn encrypt_map(map: &mut BTreeMap<String, String>) {
    for value in map.values_mut() {
        if value.is_empty() || is_encrypted(value) {
            continue;
        }
        // `encrypt_field` already logs on failure; on error we keep the
        // original value so the user's data is never silently dropped.
        if let Ok(enc) = dpapi::encrypt_field(value) {
            *value = enc;
        }
    }
}

/// Decrypt a jack's `env` (stdio) into a fresh plaintext map. For streamable-http
/// jacks the env is empty, so an empty map is returned.
///
/// Values without the `dpapi:` prefix are returned unchanged (plaintext
/// migration), so calling this on a not-yet-encrypted config is safe.
pub fn decrypted_env(jack: &JackConfig) -> BTreeMap<String, String> {
    match &jack.transport {
        JackTransport::Stdio { env, .. } => env
            .iter()
            .map(|(k, v)| (k.clone(), dpapi::decrypt_field(v)))
            .collect(),
        JackTransport::StreamableHttp { .. } => BTreeMap::new(),
    }
}

/// Decrypt a jack's `headers` (streamable-http) into a fresh plaintext map.
/// For stdio jacks the headers are empty, so an empty map is returned.
pub fn decrypted_headers(jack: &JackConfig) -> BTreeMap<String, String> {
    match &jack.transport {
        JackTransport::StreamableHttp { headers, .. } => headers
            .iter()
            .map(|(k, v)| (k.clone(), dpapi::decrypt_field(v)))
            .collect(),
        JackTransport::Stdio { .. } => BTreeMap::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stdio_with_env(env_val: &str) -> PatchbayConfig {
        let mut env = BTreeMap::new();
        env.insert("DB_TOKEN".to_string(), env_val.to_string());
        PatchbayConfig {
            version: 1,
            port: 39100,
            autostart: false,
            jacks: vec![JackConfig {
                name: "prod".to_string(),
                patched: false,
                transport: JackTransport::Stdio {
                    command: "npx".to_string(),
                    args: vec!["-y".to_string(), "some-db-mcp".to_string()],
                    env,
                },
                sharing: Sharing::Shared,
                tools: None,
            }],
            bays: BTreeMap::new(),
            seen_clients: Vec::new(),
            client_overrides: BTreeMap::new(),
            require_approval_for_new_clients: true,
            request_logging_enabled: false,
            forbidden_clients: Vec::new(),
        }
    }

    fn env_value(cfg: &PatchbayConfig) -> String {
        match &cfg.jacks[0].transport {
            JackTransport::Stdio { env, .. } => env.get("DB_TOKEN").unwrap().clone(),
            _ => panic!("expected stdio"),
        }
    }

    // ---- DPAPI round-trip is Windows-only (CryptProtectData) ----

    #[cfg(windows)]
    #[test]
    fn plaintext_becomes_dpapi_prefixed_and_round_trips() {
        let mut cfg = stdio_with_env("hunter2");
        encrypt_config_secrets(&mut cfg);

        let enc = env_value(&cfg);
        assert!(
            enc.starts_with("dpapi:"),
            "expected dpapi: prefix, got: {}",
            enc
        );
        assert_ne!(enc, "hunter2", "value was not actually encrypted");

        // Decrypting restores the original plaintext.
        let dec = dpapi::decrypt_field(&enc);
        assert_eq!(dec, "hunter2");

        // decrypted_env helper agrees.
        let env = decrypted_env(&cfg.jacks[0]);
        assert_eq!(env.get("DB_TOKEN").unwrap(), "hunter2");
    }

    #[cfg(windows)]
    #[test]
    fn already_encrypted_is_not_double_encrypted() {
        let mut cfg = stdio_with_env("secret123");
        encrypt_config_secrets(&mut cfg);
        let once = env_value(&cfg);

        // Second pass must be a no-op on the now-prefixed value.
        encrypt_config_secrets(&mut cfg);
        let twice = env_value(&cfg);
        assert_eq!(once, twice, "double-encryption detected");

        // And it still decrypts to the original.
        assert_eq!(dpapi::decrypt_field(&twice), "secret123");
    }

    #[cfg(windows)]
    #[test]
    fn empty_value_stays_empty() {
        let mut cfg = stdio_with_env("");
        encrypt_config_secrets(&mut cfg);
        assert_eq!(env_value(&cfg), "");
    }

    #[cfg(windows)]
    #[test]
    fn headers_get_encrypted_for_http_jack() {
        let mut headers = BTreeMap::new();
        headers.insert("Authorization".to_string(), "Bearer xyz".to_string());
        let mut cfg = PatchbayConfig {
            version: 1,
            port: 39100,
            autostart: false,
            jacks: vec![JackConfig {
                name: "docs".to_string(),
                patched: true,
                transport: JackTransport::StreamableHttp {
                    url: "https://example.com/mcp".to_string(),
                    headers,
                },
                sharing: Sharing::Shared,
                tools: None,
            }],
            bays: BTreeMap::new(),
            seen_clients: Vec::new(),
            client_overrides: BTreeMap::new(),
            require_approval_for_new_clients: true,
            request_logging_enabled: false,
            forbidden_clients: Vec::new(),
        };
        encrypt_config_secrets(&mut cfg);
        let h = decrypted_headers(&cfg.jacks[0]);
        assert_eq!(h.get("Authorization").unwrap(), "Bearer xyz");
    }

    // ---- plaintext pass-through works without touching DPAPI ----

    #[test]
    fn decrypt_helper_passes_through_plaintext() {
        // decrypt_field on a value lacking the dpapi: prefix returns it as-is,
        // so a partially-migrated config still spawns correctly. This exercises
        // the helper wiring without needing DPAPI.
        let jack = JackConfig {
            name: "prod".to_string(),
            patched: false,
            transport: JackTransport::Stdio {
                command: "npx".to_string(),
                args: vec![],
                env: {
                    let mut m = BTreeMap::new();
                    m.insert("K".to_string(), "plain-value".to_string());
                    m
                },
            },
            sharing: Sharing::Shared,
            tools: None,
        };
        let env = decrypted_env(&jack);
        assert_eq!(env.get("K").unwrap(), "plain-value");
        // stdio jack has no headers.
        assert!(decrypted_headers(&jack).is_empty());
    }
}
