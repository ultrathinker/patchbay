//! Persistent Patchbay configuration: load, save, validate.
//!
//! Config file: `%APPDATA%\Patchbay\patchbay.json` (pure JSON). A documented
//! `patchbay.example.jsonc` is written next to it on first run.
//!
//! (S14) Beside them sits `patchbay.state.json`, holding what Patchbay has
//! OBSERVED rather than what the user configured — currently just the list of
//! agents that have connected and when. It is split out because that list is
//! rewritten by ordinary agent traffic, which had been making the file that
//! holds every secret write-hot for no configuration reason. It is disposable:
//! delete it and previously-known agents simply look new. See [`RuntimeState`]. Save is atomic
//! (write `patchbay.json.tmp` then rename over `patchbay.json`) and encrypts
//! any plaintext secrets immediately before writing, so the on-disk invariant
//! "all secrets are `dpapi:`-prefixed" always holds. Load never crashes the
//! tray: a missing file yields the first-run template (written to disk); a
//! parse/read error logs and falls back to a safe empty default.

pub mod schema;
pub mod secrets;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::utils::log::log;

// Re-export the primary types so callers use `config::PatchbayConfig` etc.
pub use schema::{
    ClientOverride, JackConfig, JackConfigInput, JackTransport, PatchbayConfig, SeenClient, Sharing,
    UiMode,
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
        ui_mode: crate::config::UiMode::Tray,
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
        ui_mode: crate::config::UiMode::Tray,
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


// ---- runtime state (S14) --------------------------------------------------

/// The sibling file holding OBSERVED state rather than configuration.
///
/// `patchbay.json` is the user's: servers, secrets, permissions, settings. It
/// should change when the user changes something. `patchbay.state.json` is
/// Patchbay's own notebook — who has connected, and when — and is rewritten by
/// ordinary agent traffic. Keeping the two in one file meant every heartbeat
/// rewrote the secrets, and a single bad write took everything.
///
/// Losing this file is a non-event: previously-known agents look new, which at
/// worst means one approval prompt each. Nothing in it is secret, and nothing
/// in it can be reconstructed wrongly.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct RuntimeState {
    #[serde(default = "default_state_version")]
    pub version: u32,
    #[serde(default)]
    pub seen_clients: Vec<crate::config::SeenClient>,
}

fn default_state_version() -> u32 {
    1
}

/// `<dir>/<config stem>.state.json` beside the config it belongs to — derived
/// from the config path rather than fixed, so a test's isolated config gets its
/// own isolated state file and two tests cannot collide in the temp directory.
pub fn state_path_for(config_path: &Path) -> PathBuf {
    let stem = config_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "patchbay".to_string());
    config_path.with_file_name(format!("{}.state.json", stem))
}

/// Read the sibling state file, if it is there and readable.
///
/// Every failure returns `None` and is logged, never propagated: a missing or
/// damaged notebook must not stop Patchbay from loading the user's actual
/// configuration. `None` means "say nothing about seen_clients", which leaves
/// whatever the config file carried — that is what makes the migration work.
fn load_state(path: &Path) -> Option<RuntimeState> {
    if !path.exists() {
        return None;
    }
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<RuntimeState>(&text) {
            Ok(st) => Some(st),
            Err(e) => {
                log(&format!(
                    "state: {} did not parse ({}); treating known agents as new",
                    path.display(),
                    e
                ));
                None
            }
        },
        Err(e) => {
            log(&format!("state: could not read {}: {}", path.display(), e));
            None
        }
    }
}

/// Write the sibling state file, atomically (temp + rename) like the config.
fn write_state_file(cfg: &PatchbayConfig, path: &Path) -> Result<(), String> {
    let state = RuntimeState {
        version: default_state_version(),
        seen_clients: cfg.seen_clients.clone(),
    };
    let json = serde_json::to_string_pretty(&state)
        .map_err(|e| format!("state serialize: {}", e))?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create_dir_all: {}", e))?;
    }
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp_path = PathBuf::from(tmp);
    std::fs::write(&tmp_path, json).map_err(|e| format!("write {}: {}", tmp_path.display(), e))?;
    std::fs::rename(&tmp_path, path).map_err(|e| format!("rename: {}", e))
}

/// Write ONLY the observed-agents file, leaving `patchbay.json` untouched.
///
/// This is the point of the split. Recording that an agent connected, and
/// refreshing its `last_seen`, are the two things that happen because of
/// ordinary traffic rather than because the user changed anything — and they
/// change nothing outside this file. Routing them through the full [`save`]
/// would still rewrite the file holding every server definition and every
/// encrypted secret, several times an hour, to record something no one
/// configured. So they do not.
pub fn save_runtime_state(cfg: &PatchbayConfig) -> Result<(), String> {
    write_state_file(cfg, &state_path_for(&config_file_path()))
}

/// Parse a config from an arbitrary path (used by `load` and by tests).
pub fn load_from_path(path: &Path) -> Result<PatchbayConfig, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("read {}: {}", path.display(), e))?;
    let mut cfg: PatchbayConfig =
        serde_json::from_str(&text).map_err(|e| format!("parse {}: {}", path.display(), e))?;
    merge_state(&mut cfg, path);
    repair_on_load(&mut cfg);
    Ok(cfg)
}

/// Fixups applied to EVERY config that comes off disk, whichever loader read
/// it. Kept in one function so the two loaders cannot drift apart.
///
/// Today that is one thing: re-key each Custom override against the jacks that
/// actually exist. Without it a stale override map both keeps phantom entries
/// and — far worse — leaves real jacks unnamed, and an unnamed jack used to
/// fall through to the global flag, i.e. fail OPEN. It is repaired here, at the
/// boundary, so that nothing downstream has to remember to.
fn merge_state(cfg: &mut PatchbayConfig, config_path: &Path) {
    // No state file yet? Then whatever the config carried IS the state, and the
    // next save moves it across — that is the whole migration. Once the state
    // file exists it is authoritative, because the config stops writing the key
    // and would otherwise pin the list to its value at migration time forever.
    if let Some(state) = load_state(&state_path_for(config_path)) {
        cfg.seen_clients = state.seen_clients;
    } else if !cfg.seen_clients.is_empty() {
        log(&format!(
            "config: migrating {} seen agent(s) out of the config file into {}",
            cfg.seen_clients.len(),
            state_path_for(config_path).display()
        ));
    }
}

fn repair_on_load(cfg: &mut PatchbayConfig) {
    let repairs = cfg.sync_override_jacks();
    if repairs > 0 {
        log(&format!(
            "config: repaired {} stale/missing entries across {} Custom override(s) \
             (a Custom list no longer matched the jack list)",
            repairs,
            cfg.client_overrides.len()
        ));
    }
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
        Ok(mut cfg) => {
            merge_state(&mut cfg, path);
            repair_on_load(&mut cfg);
            Ok(cfg)
        }
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

    // Keep ONE generation of the file we are about to replace. This config
    // holds every server definition and every DPAPI-encrypted secret, it is
    // rewritten by ordinary background activity, and until now a single bad
    // write left nothing to go back to. A copy costs a few kilobytes and one
    // syscall; its absence costs the user their entire setup. A failure to make
    // it is logged, never fatal — refusing to save because the backup failed
    // would be the worse outcome.
    if path.exists() {
        let mut bak = path.as_os_str().to_os_string();
        bak.push(".bak");
        if let Err(e) = std::fs::copy(path, PathBuf::from(bak)) {
            log(&format!("config: could not refresh .bak before save: {}", e));
        }
    }

    std::fs::rename(&tmp_path, path).map_err(|e| format!("rename: {}", e))?;

    // The notebook, second and separately, and NON-FATALLY: refusing to switch
    // a server on because a disposable file could not be written would be a
    // worse product than forgetting who connected.
    if let Err(e) = write_state_file(cfg, &state_path_for(path)) {
        log(&format!("state: {}", e));
    }

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
            ui_mode: crate::config::UiMode::Tray,
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
            ui_mode: crate::config::UiMode::Tray,
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
            ui_mode: crate::config::UiMode::Tray,
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
            ui_mode: crate::config::UiMode::Tray,
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
            ui_mode: crate::config::UiMode::Tray,
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
            ui_mode: crate::config::UiMode::Tray,
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
    fn a_drifted_override_map_is_repaired_on_load_not_by_the_caller() {
        // Written to disk with a Custom list naming a jack that does not exist
        // and omitting the one that does — the shape a hand-edit or a rename
        // leaves behind. Every loader must hand back a repaired config, so no
        // caller has to remember to ask.
        let path = fresh_test_config_path();
        let mut cfg = first_run_template();
        cfg.jacks = vec![crate::config::JackConfig {
            name: "alpha".to_string(),
            patched: true,
            transport: crate::config::JackTransport::Stdio {
                command: "npx".to_string(),
                args: vec![],
                env: BTreeMap::new(),
            },
            sharing: crate::config::Sharing::Shared,
            tools: None,
        }];
        let mut stale = BTreeMap::new();
        stale.insert("alpha-prod".to_string(), true);
        cfg.client_overrides.insert(
            "codex".to_string(),
            crate::config::ClientOverride {
                enabled: true,
                jacks: stale,
            },
        );
        save_to_path(&cfg, &path).expect("save");

        let loaded = load_from_path(&path).expect("load");
        let ovr = &loaded.client_overrides["codex"];
        assert!(!ovr.jacks.contains_key("alpha-prod"), "stale key survived");
        assert_eq!(ovr.jacks.get("alpha"), Some(&false), "missing key not seeded closed");
        // ...and the classifying loader must agree with the plain one.
        let via_result = load_result_from_path(&path).expect("load_result");
        assert_eq!(
            via_result.client_overrides["codex"].jacks,
            ovr.jacks,
            "the two loaders disagree — exactly the drift repair_on_load exists to prevent"
        );
    }

    #[test]
    fn saving_keeps_one_generation_of_the_previous_file() {
        let path = fresh_test_config_path();
        let mut first = first_run_template();
        first.port = 39001;
        save_to_path(&first, &path).expect("first save");

        let mut second = first.clone();
        second.port = 39002;
        save_to_path(&second, &path).expect("second save");

        let bak = PathBuf::from({
            let mut s = path.as_os_str().to_os_string();
            s.push(".bak");
            s
        });
        assert!(bak.exists(), "no .bak written");
        let recovered = load_from_path(&bak).expect("the backup must itself be loadable");
        assert_eq!(recovered.port, 39001, "the .bak holds the wrong generation");
        assert_eq!(load_from_path(&path).expect("live").port, 39002);
    }

    #[test]
    fn seen_agents_live_beside_the_config_not_inside_it() {
        let path = fresh_test_config_path();
        let mut cfg = first_run_template();
        cfg.seen_clients.push(crate::config::SeenClient {
            name: "Codex".to_string(),
            first_seen_version: None,
            first_seen: "2026-07-29T16:26:20+02:00".to_string(),
            last_seen: Some("2026-09-03T11:02:00+02:00".to_string()),
        });
        save_to_path(&cfg, &path).expect("save");

        // The config file itself must not mention the agent at all: the point
        // of the split is that agent traffic stops rewriting this file.
        let on_disk = std::fs::read_to_string(&path).expect("read config");
        assert!(
            !on_disk.contains("Codex"),
            "the observed-agents list is still in the config file:\n{}",
            on_disk
        );
        // ...and it is in the sibling, and comes back on load.
        assert!(state_path_for(&path).exists(), "no state file written");
        let back = load_from_path(&path).expect("load");
        assert_eq!(back.seen_clients.len(), 1);
        assert_eq!(back.seen_clients[0].last_seen.as_deref(), Some("2026-09-03T11:02:00+02:00"));
    }

    #[test]
    fn an_existing_install_migrates_its_seen_agents_on_first_save() {
        // An old-format file: the list is INSIDE the config, and there is no
        // state file next to it. Loading must still see the agents (or every
        // known agent would suddenly need re-approving), and the first save
        // must move them across.
        let path = fresh_test_config_path();
        let old_format = r#"{
            "version": 1, "port": 39100, "jacks": [],
            "seen_clients": [ { "name": "some-agent", "first_seen": "2026-01-01T00:00:00+00:00" } ]
        }"#;
        std::fs::write(&path, old_format).expect("write old config");
        assert!(!state_path_for(&path).exists());

        let loaded = load_from_path(&path).expect("load");
        assert_eq!(
            loaded.seen_clients.len(),
            1,
            "the migration must not lose the agents the user already approved"
        );

        save_to_path(&loaded, &path).expect("save");
        assert!(state_path_for(&path).exists(), "not migrated");
        assert!(!std::fs::read_to_string(&path).unwrap().contains("some-agent"));
        assert_eq!(load_from_path(&path).expect("reload").seen_clients.len(), 1);
    }

    #[test]
    fn a_damaged_state_file_costs_only_the_agent_list() {
        // The notebook is disposable by design: if it cannot be read, Patchbay
        // must still load the user's servers and secrets normally. Anything
        // else would make a throwaway file able to take the app down.
        let path = fresh_test_config_path();
        let mut cfg = first_run_template();
        cfg.seen_clients.push(crate::config::SeenClient {
            name: "Codex".to_string(),
            first_seen_version: None,
            first_seen: String::new(),
            last_seen: None,
        });
        save_to_path(&cfg, &path).expect("save");
        std::fs::write(state_path_for(&path), "{ not json").expect("corrupt the state");

        let loaded = load_from_path(&path).expect("a damaged state file must not fail the load");
        assert_eq!(loaded.jacks.len(), cfg.jacks.len(), "the servers must survive");
        assert!(loaded.seen_clients.is_empty(), "known agents simply look new again");
    }

    #[test]
    fn the_state_file_sits_beside_its_own_config() {
        // Derived from the config path, not fixed: two tests (or a test and the
        // real app) must never share one notebook.
        let a = state_path_for(Path::new(r"C:\x\patchbay.json"));
        let b = state_path_for(Path::new(r"C:\x\patchbay_test_7.json"));
        assert_eq!(a.file_name().unwrap(), "patchbay.state.json");
        assert_eq!(b.file_name().unwrap(), "patchbay_test_7.state.json");
        assert_eq!(a.parent(), b.parent());
        assert_ne!(a, b);
    }

    #[test]
    fn safe_default_is_empty_and_valid() {
        let d = safe_default();
        assert!(d.jacks.is_empty());
        assert_eq!(d.port, DEFAULT_PORT);
        assert!(validate(&d).is_ok());
    }
}
