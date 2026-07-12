//! Level-2 request/event log for Patchbay (opt-in).
//!
//! When `config.request_logging_enabled` is on, every handled MCP request and
//! every admin/lifecycle action is appended to a per-day file under
//! `%APPDATA%\Patchbay\logs\requests\<YYYY-MM-DD>.log` (a SEPARATE subdir from
//! the Level-1 `flexi_logger` files, so the two never interleave). Each day's
//! file rolls to `<YYYY-MM-DD>_2.log`, `_3.log`, … once it passes 5 MB, and the
//! directory is capped at 20 files total (oldest by mtime deleted first).
//!
//! Why hand-rolled (not `flexi_logger`): flexi_logger's `FileSpec` names files
//! `<basename>_r<N>.log` — it has no calendar-date basename, so producing
//! `2026-07-12.log` / `2026-07-12_2.log` exactly would be fighting the library.
//! The mechanics we own here (date file, within-day size suffix, mtime
//! retention) are small and fully under test via the pure helpers
//! ([`redact_headers`], [`truncate_body`], [`is_sensitive_header`]).
//!
//! Every writer checks the LIVE config flag on every call, so toggling it from
//! the tray takes effect on the next event with no restart.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use parking_lot::Mutex;

use crate::app_state::AppState;

/// Max size of one request-log file before it rolls to the next suffix (bytes).
const MAX_REQUEST_FILE_BYTES: u64 = 5 * 1024 * 1024;

/// Hard cap on the number of `.log` files kept in the `requests/` dir. When a
/// write would push the count over this, the oldest file(s) by mtime are
/// deleted first so the directory never grows without bound.
const MAX_REQUEST_FILES: usize = 20;

/// Serializes all request/event writes so the size check + suffix bump + mtime
/// retention can't race between threads (same pattern as the old Level-1 writer).
static WRITE_LOCK: Mutex<()> = Mutex::new(());

/// Resolve the Level-2 requests directory `%APPDATA%\Patchbay\logs\requests`,
/// creating it (and the parent logs dir) on demand.
pub fn requests_dir() -> PathBuf {
    let mut d = crate::utils::log::logs_dir();
    d.push("requests");
    let _ = std::fs::create_dir_all(&d);
    d
}

// ---- pure, testable helpers ----------------------------------------------

/// Whether a header NAME (any casing) carries a secret and must have its VALUE
/// redacted. Case-insensitive: equals `authorization`/`cookie`/`set-cookie`, or
/// CONTAINS `token`, `key`, `secret`, `auth`, or `password` as a substring (so
/// `x-api-key`, `x-token`, `proxy-authorization`, `x-secret` all match). Split
/// out so the case-insensitive matching is unit-testable without constructing a
/// `HeaderMap` (which canonicalizes names to lowercase anyway).
pub fn is_sensitive_header(name: &str) -> bool {
    let lname = name.to_ascii_lowercase();
    lname == "authorization"
        || lname == "cookie"
        || lname == "set-cookie"
        || lname.contains("token")
        || lname.contains("key")
        || lname.contains("secret")
        || lname.contains("auth")
        || lname.contains("password")
}

/// Whether a JSON object KEY (any casing) carries a secret and must have its
/// VALUE redacted when logging a request body. Same substrings as
/// [`is_sensitive_header`] plus `authorization`/`bearer`, since request bodies
/// (e.g. `patchbay__add_jack` params carrying a jack's own `headers`/`env`
/// map, or a tool call's `arguments`) commonly nest secrets under JSON keys
/// rather than HTTP headers.
fn is_sensitive_json_key(key: &str) -> bool {
    let lkey = key.to_ascii_lowercase();
    lkey.contains("token")
        || lkey.contains("key")
        || lkey.contains("secret")
        || lkey.contains("auth")
        || lkey.contains("password")
        || lkey.contains("cookie")
        || lkey.contains("bearer")
}

/// Recursively redact sensitive VALUES in a JSON body before it's logged.
/// Walks objects and arrays; any object value whose KEY matches
/// [`is_sensitive_json_key`] is replaced with `"<redacted>"` (a string, not a
/// nested structure, since we don't know if the real value was a string). Keys
/// themselves, and non-sensitive values, are left untouched. This covers the
/// case `redact_headers` cannot: a secret that arrives inside the JSON-RPC
/// `params` body (e.g. a jack's `headers`/`env` map passed to
/// `patchbay__add_jack`) rather than as an actual HTTP header on the request.
pub fn redact_body(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                if is_sensitive_json_key(k) {
                    out.insert(k.clone(), serde_json::Value::String("<redacted>".to_string()));
                } else {
                    out.insert(k.clone(), redact_body(v));
                }
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(redact_body).collect())
        }
        other => other.clone(),
    }
}

/// Turn a request's headers into `(name, value)` pairs with sensitive VALUES
/// replaced by `<redacted>`. Keys are NEVER redacted. Non-sensitive headers
/// (e.g. `X-Patchbay-Client`, `Content-Type`) pass through verbatim. A value
/// that isn't valid UTF-8 (obs-text) is shown as `<binary>`.
pub fn redact_headers(headers: &axum::http::HeaderMap) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (name, value) in headers.iter() {
        let name_s = name.as_str().to_string();
        let value_s = if is_sensitive_header(name.as_str()) {
            "<redacted>".to_string()
        } else {
            value.to_str().unwrap_or("<binary>").to_string()
        };
        out.push((name_s, value_s));
    }
    out
}

/// Truncate a serialized body/params string to the first 300 CHARACTERS. At or
/// under 300 chars → unchanged; over 300 → first 300 chars + the suffix
/// `... (total N chars)` where N is the full character count. Char-based (not
/// byte-based) so multibyte JSON is split on a character boundary, not mid-code
/// point.
pub fn truncate_body(s: &str) -> String {
    const MAX: usize = 300;
    let total = s.chars().count();
    if total <= MAX {
        s.to_string()
    } else {
        let head: String = s.chars().take(MAX).collect();
        format!("{}... (total {} chars)", head, total)
    }
}

// ---- public writers (check the live config flag first) -------------------

/// Log one handled MCP request: timestamp + resolved client identity + JSON-RPC
/// method + redacted headers + truncated body/params. No-op when the flag is off
/// (checked live). `body` is the request's JSON-RPC `params` (or `None`).
pub fn log_request(
    state: &AppState,
    client_name: Option<&str>,
    method: &str,
    headers: &axum::http::HeaderMap,
    body: Option<&serde_json::Value>,
) {
    if !state.config.read().request_logging_enabled {
        return;
    }
    let now = chrono::Local::now();
    let ts = now.format("%Y-%m-%d %H:%M:%S%.3f");
    let client = client_name.unwrap_or("-");
    let hdrs: Vec<String> = redact_headers(headers)
        .iter()
        .map(|(k, v)| format!("{}: {}", k, v))
        .collect();
    let hdrs_s = hdrs.join("; ");
    // Redact sensitive JSON keys (e.g. a jack's `headers`/`env` map passed to
    // patchbay__add_jack, or a bearer token in a tool call's `arguments`)
    // BEFORE truncating — redact_headers only covers actual HTTP headers, not
    // secrets nested in the JSON-RPC params body itself.
    let body_s = match body {
        Some(v) => truncate_body(&redact_body(v).to_string()),
        None => String::new(),
    };
    write_line(&format!(
        "[{}] [REQUEST] client={} method={} headers=[{}] body={}",
        ts, client, method, hdrs_s, body_s
    ));
}

/// Log an admin/lifecycle `[EVENT]` line (add/remove/toggle jack, allow/forbid
/// a client, copy URL, reload config, gateway rebind, app start/stop, …). No-op
/// when the flag is off. Call this AFTER the action has actually completed (e.g.
/// after a successful `config::save`), not before.
pub fn log_event(state: &AppState, msg: &str) {
    if !state.config.read().request_logging_enabled {
        return;
    }
    write_line(&format_line("EVENT", msg));
}

/// Log an `[ERROR]` line within the request log (e.g. a failed jack start).
/// No-op when the flag is off.
pub fn log_error(state: &AppState, msg: &str) {
    if !state.config.read().request_logging_enabled {
        return;
    }
    write_line(&format_line("ERROR", msg));
}

fn format_line(kind: &str, msg: &str) -> String {
    let now = chrono::Local::now();
    format!("[{}] [{}] {}", now.format("%Y-%m-%d %H:%M:%S%.3f"), kind, msg)
}

// ---- hand-rolled per-day writer + retention ------------------------------

/// Append one line to today's active request-log file, rolling to a new
/// `_<N>.log` suffix when the current file is at/over [`MAX_REQUEST_FILE_BYTES`]
/// and pruning the directory to [`MAX_REQUEST_FILES`] afterward. All under
/// [`WRITE_LOCK`] so concurrent writers can't race the size check or the suffix
/// bump. Best-effort: any IO error is silently dropped (Level 2 is diagnostic).
fn write_line(line: &str) {
    let _g = WRITE_LOCK.lock();
    let dir = requests_dir();
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();

    let active_num = scan_today_numbers(&dir, &today).into_iter().max();
    let path = match active_num {
        None => num_path(&dir, &today, 1),
        Some(n) => {
            let active = num_path(&dir, &today, n);
            if file_size(&active) >= MAX_REQUEST_FILE_BYTES {
                num_path(&dir, &today, n + 1)
            } else {
                active
            }
        }
    };

    append_line(&path, line);
    enforce_retention(&dir);
}

/// Path for a given day + sequence number. `1` is the base file
/// (`<today>.log`, no suffix); `2..` are `<today>_<N>.log`.
fn num_path(dir: &Path, today: &str, n: u32) -> PathBuf {
    if n <= 1 {
        dir.join(format!("{}.log", today))
    } else {
        dir.join(format!("{}_{}.log", today, n))
    }
}

/// The set of sequence numbers present for `today`: base file counts as `1`,
/// `<today>_N.log` counts as `N` (N>=2). Used to find the highest-numbered file
/// (the active one) without assuming any particular creation order on disk.
fn scan_today_numbers(dir: &Path, today: &str) -> Vec<u32> {
    let mut nums = Vec::new();
    if dir.join(format!("{}.log", today)).exists() {
        nums.push(1);
    }
    let prefix = format!("{}_", today);
    let suffix = ".log";
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(&prefix) && name.ends_with(suffix) {
                let mid = &name[prefix.len()..name.len() - suffix.len()];
                if let Ok(n) = mid.parse::<u32>() {
                    if n >= 2 {
                        nums.push(n);
                    }
                }
            }
        }
    }
    nums
}

fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn append_line(path: &Path, line: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = f.write_all(line.as_bytes());
        let _ = f.write_all(b"\n");
    }
}

/// Keep at most [`MAX_REQUEST_FILES`] `.log` files in the directory. If there
/// are more, delete the oldest by mtime (tiebreak by path) until the cap holds.
fn enforce_retention(dir: &Path) {
    let mut files: Vec<(PathBuf, SystemTime)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("log") {
                let mtime = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::UNIX_EPOCH);
                files.push((path, mtime));
            }
        }
    }
    if files.len() <= MAX_REQUEST_FILES {
        return;
    }
    // Oldest first (mtime asc, path tiebreak) so `.take(to_remove)` drops the
    // oldest generations.
    files.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    let to_remove = files.len() - MAX_REQUEST_FILES;
    for (p, _) in files.iter().take(to_remove) {
        let _ = std::fs::remove_file(p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderName, HeaderValue};

    /// Insert helper: HeaderName is case-insensitive (canonicalized to
    /// lowercase on insert), so "Authorization" and "authorization" are the
    /// same key.
    fn insert(map: &mut HeaderMap, name: &str, val: &str) {
        map.insert(
            HeaderName::try_from(name).unwrap(),
            HeaderValue::try_from(val).unwrap(),
        );
    }

    // ---- header sensitivity (case-insensitive) ----

    #[test]
    fn sensitive_header_detection_case_insensitive() {
        // Redacted: authorization (any casing), contains "token", contains "key".
        assert!(is_sensitive_header("Authorization"));
        assert!(is_sensitive_header("AUTHORIZATION"));
        assert!(is_sensitive_header("authorization"));
        assert!(is_sensitive_header("X-Api-Key"));
        assert!(is_sensitive_header("x-api-key"));
        assert!(is_sensitive_header("x-token"));
        assert!(is_sensitive_header("Bearer-Token"));
        assert!(is_sensitive_header("apikey")); // contains "key"
        // Not redacted.
        assert!(!is_sensitive_header("X-Patchbay-Client"));
        assert!(!is_sensitive_header("x-patchbay-client"));
        assert!(!is_sensitive_header("Content-Type"));
        assert!(!is_sensitive_header("Accept"));
        assert!(!is_sensitive_header("Mcp-Session-Id"));
        // Newly covered (review fix): cookie/set-cookie/secret/auth/password.
        assert!(is_sensitive_header("Cookie"));
        assert!(is_sensitive_header("cookie"));
        assert!(is_sensitive_header("Set-Cookie"));
        assert!(is_sensitive_header("X-Secret"));
        assert!(is_sensitive_header("Proxy-Authorization"));
        assert!(is_sensitive_header("X-Password"));
    }

    #[test]
    fn redact_body_redacts_nested_sensitive_keys_keeps_others() {
        let body = serde_json::json!({
            "name": "my-jack",
            "headers": {
                "Authorization": "Bearer secret-token",
                "X-Custom": "fine"
            },
            "env": {
                "DB_PASSWORD": "hunter2",
                "API_KEY": "abc123",
                "PLAIN": "visible"
            },
            "nested": {
                "list": [
                    {"cookie": "sess=abc"},
                    {"ok": "value"}
                ]
            }
        });
        let redacted = redact_body(&body);
        assert_eq!(redacted["name"], "my-jack", "non-sensitive top-level key untouched");
        assert_eq!(redacted["headers"]["Authorization"], "<redacted>");
        assert_eq!(redacted["headers"]["X-Custom"], "fine");
        assert_eq!(redacted["env"]["DB_PASSWORD"], "<redacted>");
        assert_eq!(redacted["env"]["API_KEY"], "<redacted>");
        assert_eq!(redacted["env"]["PLAIN"], "visible");
        assert_eq!(redacted["nested"]["list"][0]["cookie"], "<redacted>");
        assert_eq!(redacted["nested"]["list"][1]["ok"], "value");
    }

    #[test]
    fn redact_headers_redacts_values_keeps_keys() {
        let mut map = HeaderMap::new();
        insert(&mut map, "authorization", "Bearer hunter2");
        insert(&mut map, "X-Api-Key", "abc123");
        insert(&mut map, "X-Patchbay-Client", "claude-work");
        insert(&mut map, "Content-Type", "application/json");

        let pairs = redact_headers(&map);
        let m: std::collections::HashMap<String, String> = pairs.into_iter().collect();

        // Sensitive values replaced with <redacted>; keys preserved (HeaderMap
        // canonicalizes names to lowercase on insert).
        assert_eq!(m.get("authorization"), Some(&"<redacted>".to_string()));
        assert_eq!(m.get("x-api-key"), Some(&"<redacted>".to_string()));
        // Non-sensitive pass through verbatim.
        assert_eq!(m.get("x-patchbay-client"), Some(&"claude-work".to_string()));
        assert_eq!(m.get("content-type"), Some(&"application/json".to_string()));
    }

    // ---- truncation ----

    #[test]
    fn truncate_under_300_is_unchanged() {
        assert_eq!(truncate_body("hello"), "hello");
        assert_eq!(truncate_body(""), "");
    }

    #[test]
    fn truncate_exactly_300_has_no_suffix() {
        let s: String = std::iter::repeat('a').take(300).collect();
        let t = truncate_body(&s);
        assert_eq!(t.chars().count(), 300);
        assert_eq!(t, s, "exact-300 must be returned verbatim");
        assert!(!t.contains("... (total"));
    }

    #[test]
    fn truncate_over_300_keeps_first_300_plus_suffix() {
        let s: String = std::iter::repeat('a').take(400).collect();
        let t = truncate_body(&s);
        assert!(
            t.starts_with(&"a".repeat(300)),
            "must begin with the first 300 chars"
        );
        assert!(
            t.ends_with("... (total 400 chars)"),
            "must end with the total-count suffix, got: {t}"
        );
    }

    #[test]
    fn truncate_is_char_based_not_byte_based() {
        // 150 smileys = 450 bytes but 150 chars -> under the 300-CHAR cap, so
        // no truncation/suffix. (A byte-based limit would have truncated.)
        let s: String = std::iter::repeat('\u{1F600}').take(150).collect();
        let t = truncate_body(&s);
        assert_eq!(t, s, "150 chars (450 bytes) must not be truncated");
        assert!(!t.contains("... (total"));
    }
}
