//! Persistent Patchbay configuration: load, save, validate.
//!
//! Config file: `%APPDATA%\Patchbay\patchbay.json` (pure JSON). A documented
//! `patchbay.example.jsonc` is written next to it on first run. Save is atomic
//! (write `patchbay.json.tmp` then rename over `patchbay.json`) and encrypts
//! any plaintext secrets immediately before writing, so the on-disk invariant
//! "all secrets are `dpapi:`-prefixed" always holds. Load never crashes the
//! tray: a missing file yields the first-run template (written to disk); a
//! parse/read error logs and falls back to a safe empty default.

pub mod schema;
pub mod secrets;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::utils::log::log;

// Re-export the primary types so callers use `config::PatchbayConfig` etc.
pub use schema::{
    ClientOverride, JackConfig, JackConfigInput, JackTransport, PatchbayConfig, SeenClient, Sharing,
};
// Re-export the default port constant for callers (e.g. gateway bind).
pub use schema::{CURRENT_VERSION, DEFAULT_PORT};

/// Directory holding the config: `%APPDATA%\Patchbay`.
pub fn config_dir() -> PathBuf {
    let mut dir = dirs::config_dir().unwrap_or_else(std::env::temp_dir);
    dir.push("Patchbay");
    dir
}

/// Path to the real config file: `<config_dir>/patchbay.json`.
pub fn config_file_path() -> PathBuf {
    // (S10 test isolation) state-mutating unit tests route `config::save` at a
    // throwaway temp path via [`set_test_config_path`] so they never touch the
    // user's real `%APPDATA%\Patchbay\patchbay.json`. `None` in normal builds.
    //
    // THREAD-LOCAL, not a shared global: `cargo test` runs different test
    // functions concurrently on different OS threads by default. A single
    // global override would let one test's `isolate_config()` clobber
    // another's mid-write, causing cross-test file races (observed live as
    // `rename: The system cannot find the file specified` when two tests'
    // writes/renames interleaved against whichever path happened to be
    // globally set at that instant). Each thread gets its own override.
    #[cfg(test)]
    {
        if let Some(p) = TEST_CONFIG_PATH.with(|c| c.borrow().clone()) {
            return p;
        }
    }
    let mut p = config_dir();
    p.push("patchbay.json");
    p
}

#[cfg(test)]
thread_local! {
    static TEST_CONFIG_PATH: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
static TEST_PATH_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// (Test hook) Redirect `config_file_path()` to `path` (or back to the real
/// path with `None`) so state-mutating tests never write the real config.
/// Thread-local: only affects the calling thread (see the note above).
#[cfg(test)]
pub fn set_test_config_path(path: Option<PathBuf>) {
    TEST_CONFIG_PATH.with(|c| *c.borrow_mut() = path);
}

/// (Test hook) A fresh, unique temp path for one test's isolated config writes.
#[cfg(test)]
pub fn fresh_test_config_path() -> PathBuf {
    use std::sync::atomic::Ordering;
    let n = TEST_PATH_COUNTER.fetch_add(1, Ordering::SeqCst);
    let mut p = std::env::temp_dir();
    p.push(format!("patchbay_test_{}_{}.json", std::process::id(), n));
    p
}

/// Path to the documented example: `<config_dir>/patchbay.example.jsonc`.
pub fn example_file_path() -> PathBuf {
    let mut p = config_dir();
    p.push("patchbay.example.jsonc");
    p
}

/// A safe, empty default used when the on-disk config can't be read/parsed.
/// No jacks, default port — the tray still works, just nothing is patched.
pub fn safe_default() -> PatchbayConfig {
    PatchbayConfig {
        version: CURRENT_VERSION,
        port: DEFAULT_PORT,
        autostart: false,
        jacks: Vec::new(),
        bays: BTreeMap::new(),
        seen_clients: Vec::new(),
        client_overrides: BTreeMap::new(),
        require_approval_for_new_clients: false,
        request_logging_enabled: false,
        forbidden_clients: Vec::new(),
    }
}

/// The first-run template: one example stdio jack `prod`, **patched off**
/// (MASTER_PLAN D4: prod ships `patched:false`). Its `DB_TOKEN` is plaintext
/// in memory and gets `dpapi:`-wrapped on the first save (see `save`).
pub fn first_run_template() -> PatchbayConfig {
    let mut env = BTreeMap::new();
    env.insert("DB_TOKEN".to_string(), "REPLACE_ME".to_string());

    let prod = JackConfig {
        name: "prod".to_string(),
        patched: false,
        transport: JackTransport::Stdio {
            command: "npx".to_string(),
            args: vec!["-y".to_string(), "some-db-mcp".to_string()],
            env,
        },
        sharing: Sharing::Shared,
        tools: None,
    };

    PatchbayConfig {
        version: CURRENT_VERSION,
        port: DEFAULT_PORT,
        autostart: false,
        jacks: vec![prod],
        bays: BTreeMap::new(),
        seen_clients: Vec::new(),
        client_overrides: BTreeMap::new(),
        require_approval_for_new_clients: false,
        request_logging_enabled: false,
        forbidden_clients: Vec::new(),
    }
}

/// Documented example written next to the real config on first run. Purely
/// informational (Patchbay never reads it); the real file stays valid JSON.
const EXAMPLE_JSONC: &str = include_str!("patchbay.example.jsonc");

/// Write the documented example file (best-effort; failures are logged).
fn write_example_jsonc() {
    let path = example_file_path();
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            log(&format!("config: could not create dir for example: {}", e));
            return;
        }
    }
    if let Err(e) = std::fs::write(&path, EXAMPLE_JSONC) {
        log(&format!("config: could not write example jsonc: {}", e));
    }
}

/// Load the config from the default path.
///
/// - File missing → write the first-run template (+ example jsonc) and return
///   it.
/// - Read/parse error → log and return `safe_default()` (do not crash tray).
pub fn load() -> PatchbayConfig {
    let path = config_file_path();
    if !path.exists() {
        log("config: file not found, writing first-run template");
        let template = first_run_template();
        if let Err(e) = save(&template) {
            log(&format!("config: failed to write first-run template: {}", e));
        }
        write_example_jsonc();
        return template;
    }
    match load_from_path(&path) {
        Ok(cfg) => {
            log(&format!(
                "config: loaded {} jacks from {}",
                cfg.jacks.len(),
                path.display()
            ));
            cfg
        }
        Err(e) => {
            log(&format!("config: load failed ({}), using safe default", e));
            safe_default()
        }
    }
}

/// Parse a config from an arbitrary path (used by `load` and by tests).
pub fn load_from_path(path: &Path) -> Result<PatchbayConfig, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("read {}: {}", path.display(), e))?;
    let cfg: PatchbayConfig =
        serde_json::from_str(&text).map_err(|e| format!("parse {}: {}", path.display(), e))?;
    Ok(cfg)
}

/// Outcome of [`load_result`]: distinguishes a missing config (first run) from a
/// corrupt one (parse/IO error), so a corrupt, hand-edited file is NEVER
/// silently overwritten with a safe default.
#[derive(Debug, Clone)]
pub enum ConfigError {
    /// No config file on disk yet (first run) — caller may write the template.
    Missing,
    /// The file exists but could not be parsed.
    Parse(String),
    /// The file exists but could not be read.
    Io(String),
}

impl ConfigError {
    /// Human-readable reason (for logging / the tray tooltip).
    pub fn reason(&self) -> String {
        match self {
            ConfigError::Missing => "config file missing".to_string(),
            ConfigError::Parse(s) => s.clone(),
            ConfigError::Io(s) => s.clone(),
        }
    }
}

/// Load without any first-run side effects, classifying the outcome so callers
/// can avoid overwriting a corrupt config.
/// - `Ok(cfg)` — loaded cleanly.
/// - `Err(Missing)` — file absent (first run): caller writes the template.
/// - `Err(Parse|Io)` — file present but unusable: MUST NOT be overwritten.
pub fn load_result() -> Result<PatchbayConfig, ConfigError> {
    load_result_from_path(&config_file_path())
}

/// Path-parameterized variant of [`load_result`] (used by `load_result` + tests).
pub fn load_result_from_path(path: &Path) -> Result<PatchbayConfig, ConfigError> {
    if !path.exists() {
        return Err(ConfigError::Missing);
    }
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => return Err(ConfigError::Io(format!("read {}: {}", path.display(), e))),
    };
    match serde_json::from_str::<PatchbayConfig>(&text) {
        Ok(cfg) => Ok(cfg),
        Err(e) => Err(ConfigError::Parse(format!("parse {}: {}", path.display(), e))),
    }
}

/// Save the config to the default path, atomically, encrypting any plaintext
/// secrets first. The in-memory `cfg` is untouched (the caller may still hold
/// plaintext); only the on-disk copy is wrapped.
pub fn save(cfg: &PatchbayConfig) -> Result<(), String> {
    save_to_path(cfg, &config_file_path())
}

/// Save to an arbitrary path (used by `save` and by tests). Atomic via
/// `<path>.tmp` + rename. Encrypts plaintext secrets before writing.
pub fn save_to_path(cfg: &PatchbayConfig, path: &Path) -> Result<(), String> {
    let mut to_write = cfg.clone();
    secrets::encrypt_config_secrets(&mut to_write);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create_dir_all: {}", e))?;
    }

    let json = serde_json::to_string_pretty(&to_write)
        .map_err(|e| format!("serialize: {}", e))?;

    // Atomic write: write a sibling temp file, then rename over the target.
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp_path = PathBuf::from(tmp);

    std::fs::write(&tmp_path, json).map_err(|e| format!("write {}: {}", tmp_path.display(), e))?;
    std::fs::rename(&tmp_path, path).map_err(|e| format!("rename: {}", e))?;

    Ok(())
}

/// Validate jack names against the rules used for namespacing
/// (`<jack>__<tool>`):
/// - non-empty,
/// - charset `^[A-Za-z0-9_-]+$`,
/// - contains no `__` (would break the namespace split),
/// - length <= 40,
/// - unique within the config.
///
/// Returns `Ok(())` if clean, or `Err(Vec<String>)` listing **all** violations
/// (so the user sees every problem at once).
pub fn validate(cfg: &PatchbayConfig) -> Result<(), Vec<String>> {
    let mut errors: Vec<String> = Vec::new();
    let mut counts: BTreeMap<String, u32> = BTreeMap::new();

    for (i, jack) in cfg.jacks.iter().enumerate() {
        let name = jack.name.as_str();

        if name.is_empty() {
            errors.push(format!("jack[{}]: name is empty", i));
        } else {
            if name.len() > 40 {
                errors.push(format!(
                    "jack[{}]: name {:?} is {} chars (max 40)",
                    i,
                    name,
                    name.len()
                ));
            }
            if name.contains("__") {
                errors.push(format!(
                    "jack[{}]: name {:?} contains reserved separator '__'",
                    i,
                    name
                ));
            }
            if !is_valid_name_charset(name) {
                errors.push(format!(
                    "jack[{}]: name {:?} has invalid chars (allowed: A-Z a-z 0-9 _ -)",
                    i,
                    name
                ));
            }
        }

        *counts.entry(jack.name.clone()).or_insert(0) += 1;
    }

    for (name, count) in &counts {
        if *count > 1 {
            errors.push(format!("jack name {:?} appears {} times (must be unique)", name, count));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// `^[A-Za-z0-9_-]+$` (empty handled by caller).
fn is_valid_name_charset(name: &str) -> bool {
    name.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Per-jack name validity (the per-name subset of [`validate`], NOT duplicate-
/// aware): non-empty, `^[A-Za-z0-9_-]+$`, no `__`, <= 40 chars. Used to skip
/// invalid jacks from the start pipeline and the tools/list merge.
pub fn is_valid_jack_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 40
        && !name.contains("__")
        && is_valid_name_charset(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jack(name: &str, patched: bool) -> JackConfig {
        JackConfig {
            name: name.to_string(),
            patched,
            transport: JackTransport::Stdio {
                command: "npx".to_string(),
                args: vec!["-y".to_string(), "some-db-mcp".to_string()],
                // empty env so save's encrypt step is a no-op -> exact round-trip
                env: BTreeMap::new(),
            },
            sharing: Sharing::Shared,
            tools: None,
        }
    }

    fn http_jack(name: &str) -> JackConfig {
        JackConfig {
            name: name.to_string(),
            patched: true,
            transport: JackTransport::StreamableHttp {
                url: "https://example.com/mcp".to_string(),
                headers: BTreeMap::new(),
            },
            sharing: Sharing::PerClientSession,
            tools: None,
        }
    }

    // ---- round trip: save -> load yields an equal config ----

    #[test]
    fn save_load_round_trip() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "patchbay_test_roundtrip_{}_{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        let cfg = PatchbayConfig {
            version: CURRENT_VERSION,
            port: 39100,
            autostart: true,
            jacks: vec![jack("alpha", true), http_jack("beta")],
            bays: BTreeMap::new(),
            seen_clients: Vec::new(),
            client_overrides: BTreeMap::new(),
            require_approval_for_new_clients: false,
            request_logging_enabled: false,
            forbidden_clients: Vec::new(),
        };

        save_to_path(&cfg, &path).expect("save should succeed");
        let loaded = load_from_path(&path).expect("load should succeed");

        assert_eq!(cfg, loaded, "config should survive a save/load round trip");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_is_atomic_no_tmp_left_behind() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "patchbay_test_atomic_{}_{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        save_to_path(&safe_default(), &path).expect("save");
        assert!(path.exists(), "target file should exist");
        let tmp = {
            let mut t = path.as_os_str().to_os_string();
            t.push(".tmp");
            PathBuf::from(t)
        };
        assert!(!tmp.exists(), "temp file should have been renamed away");
        let _ = std::fs::remove_file(&path);
    }

    // ---- validation ----

    #[test]
    fn validation_passes_for_clean_config() {
        let cfg = PatchbayConfig {
            version: CURRENT_VERSION,
            port: DEFAULT_PORT,
            autostart: false,
            jacks: vec![jack("prod", false), jack("docs-v2", true), http_jack("api")],
            bays: BTreeMap::new(),
            seen_clients: Vec::new(),
            client_overrides: BTreeMap::new(),
            require_approval_for_new_clients: false,
            request_logging_enabled: false,
            forbidden_clients: Vec::new(),
        };
        assert!(validate(&cfg).is_ok(), "clean config should validate");
    }

    #[test]
    fn validation_reports_every_violation() {
        let cfg = PatchbayConfig {
            version: CURRENT_VERSION,
            port: DEFAULT_PORT,
            autostart: false,
            jacks: vec![
                jack("", false),              // empty
                jack("a__b", true),           // contains __
                jack("dup", false),
                jack("dup", false),           // duplicate
                jack(&"x".repeat(41), true),  // > 40 chars
                jack("bad name", false),      // invalid charset (space)
            ],
            bays: BTreeMap::new(),
            seen_clients: Vec::new(),
            client_overrides: BTreeMap::new(),
            require_approval_for_new_clients: false,
            request_logging_enabled: false,
            forbidden_clients: Vec::new(),
        };
        let errs = validate(&cfg).expect_err("should have violations");

        assert!(
            errs.iter().any(|e| e.contains("empty")),
            "expected empty-name violation: {:?}",
            errs
        );
        assert!(
            errs.iter().any(|e| e.contains("__")),
            "expected '__' violation: {:?}",
            errs
        );
        assert!(
            errs.iter().any(|e| e.contains("unique") && e.contains("dup")),
            "expected duplicate violation: {:?}",
            errs
        );
        assert!(
            errs.iter().any(|e| e.contains("40")),
            "expected length violation: {:?}",
            errs
        );
        assert!(
            errs.iter().any(|e| e.contains("invalid chars")),
            "expected charset violation: {:?}",
            errs
        );
    }

    #[test]
    fn validation_underscore_without_double_is_ok() {
        // A single underscore is fine; only "__" is reserved.
        let cfg = PatchbayConfig {
            version: CURRENT_VERSION,
            port: DEFAULT_PORT,
            autostart: false,
            jacks: vec![jack("my_jack", true)],
            bays: BTreeMap::new(),
            seen_clients: Vec::new(),
            client_overrides: BTreeMap::new(),
            require_approval_for_new_clients: false,
            request_logging_enabled: false,
            forbidden_clients: Vec::new(),
        };
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn validation_max_length_boundary() {
        let name_40 = "a".repeat(40);
        let name_41 = "b".repeat(41);
        let ok = PatchbayConfig {
            version: CURRENT_VERSION,
            port: DEFAULT_PORT,
            autostart: false,
            jacks: vec![jack(&name_40, true)],
            bays: BTreeMap::new(),
            seen_clients: Vec::new(),
            client_overrides: BTreeMap::new(),
            require_approval_for_new_clients: false,
            request_logging_enabled: false,
            forbidden_clients: Vec::new(),
        };
        assert!(validate(&ok).is_ok(), "40 chars should be allowed");

        let bad = PatchbayConfig {
            version: CURRENT_VERSION,
            port: DEFAULT_PORT,
            autostart: false,
            jacks: vec![jack(&name_41, true)],
            bays: BTreeMap::new(),
            seen_clients: Vec::new(),
            client_overrides: BTreeMap::new(),
            require_approval_for_new_clients: false,
            request_logging_enabled: false,
            forbidden_clients: Vec::new(),
        };
        assert!(validate(&bad).is_err(), "41 chars should be rejected");
    }

    // ---- first-run template ----

    #[test]
    fn first_run_template_has_prod_off() {
        let t = first_run_template();
        assert_eq!(t.port, DEFAULT_PORT);
        assert_eq!(t.jacks.len(), 1);
        let prod = &t.jacks[0];
        assert_eq!(prod.name, "prod");
        assert!(!prod.patched, "prod must ship patched:false");
        match &prod.transport {
            JackTransport::Stdio { command, args, env } => {
                assert_eq!(command, "npx");
                assert_eq!(args, &["-y".to_string(), "some-db-mcp".to_string()]);
                assert!(env.contains_key("DB_TOKEN"));
            }
            _ => panic!("prod should be a stdio jack"),
        }
    }

    #[test]
    fn safe_default_is_empty_and_valid() {
        let d = safe_default();
        assert!(d.jacks.is_empty());
        assert_eq!(d.port, DEFAULT_PORT);
        assert!(validate(&d).is_ok());
    }
}
