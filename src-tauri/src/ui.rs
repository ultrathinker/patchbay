//! The window UI's data contract and IPC surface (S13,
//! `_planning/WINDOW_UI_PLAN.md` §7).
//!
//! Two directions, both deliberately thin:
//!
//! - **backend → frontend**: ONE type, [`UiSnapshot`], carrying the entire
//!   visible state. Fetched once when the popover opens, then pushed by
//!   [`crate::app_state::AppState::notify_state_changed`] whenever anything
//!   changes, from any source. No polling, no second event type, no partial
//!   updates to keep in sync.
//! - **frontend → backend**: `#[tauri::command]` wrappers that each call
//!   exactly ONE existing `AppState` method. **No business logic lives here.**
//!   The window is a second view, never a second model (W-D4).
//!
//! ## The absolute rule: no secrets cross this boundary
//! [`UiSnapshot`] carries names, flags, counts and statuses. It carries no jack
//! `env` values, no `headers`, no URL, and no `dpapi:` blobs — not even in
//! encrypted form. Secrets travel one way only: they may be typed into the
//! "Add server" form and handed to [`crate::app_state::AppState::add_jack`],
//! which persists them DPAPI-encrypted; nothing sends them back. Enforced by
//! `snapshot_never_leaks_secrets` below and documented in `docs/security.md`.

use serde::Serialize;
use tauri::{AppHandle, Manager};

use crate::app_state::{AppState, ToggleResult};
use crate::config::UiMode;
use crate::utils::log::log;

/// Event name the popover listens on. A single channel for the whole snapshot.
pub const STATE_EVENT: &str = "patchbay://state";

// ---------------------------------------------------------------------------
// DTOs
// ---------------------------------------------------------------------------

/// Everything the window renders, in one immutable message.
#[derive(Serialize, Clone, Debug)]
pub struct UiSnapshot {
    pub gateway: UiGateway,
    pub jacks: Vec<UiJack>,
    pub agents: Vec<UiAgent>,
    pub settings: UiSettings,
}

#[derive(Serialize, Clone, Debug)]
pub struct UiGateway {
    /// `starting` | `running` | `failed` | `stopped`.
    pub status: String,
    /// Present only when `status == "failed"`.
    pub error: Option<String>,
    pub port: u16,
    pub url: String,
    /// Live, initialized MCP sessions right now.
    pub session_count: usize,
    /// A config-file parse/IO error surfaced to the user, if any. Rendered as a
    /// persistent strip, not a toast: it does not go away by itself.
    pub config_error: Option<String>,
}

#[derive(Serialize, Clone, Debug)]
pub struct UiJack {
    pub name: String,
    pub patched: bool,
    /// `stdio` | `streamable_http`.
    pub transport: String,
    /// `running` | `starting` | `stopped` | `failed: <reason>` | `unknown`.
    pub status: String,
    /// The failure reason split out of `status`, so the row can render it
    /// without parsing a string in JavaScript.
    pub error: Option<String>,
    /// Cached tool count. `None` — NOT zero — when the jack is not running, so
    /// the UI can say "off" instead of the lie "0 tools" (plan §3.1).
    pub tool_count: Option<usize>,
    /// Whether turning this jack ON deserves an inline confirmation (plan
    /// §4.4). See [`is_sensitive_jack_name`].
    pub sensitive: bool,
}

/// How a client relates to the per-agent "Custom permissions" list.
///
/// Three states, not two, because the backend has three — and plan v1 got this
/// wrong by assuming two (see `WINDOW_UI_PLAN.md` §3.3). `enable_custom_client`
/// seeds a fresh override from the global list ONLY when no entry exists;
/// `disable_custom_client` keeps the entry and its jack map. So an agent whose
/// Custom mode is off may still be carrying a preserved list that would come
/// back on re-enable — and the UI must show THAT, not the global values it
/// would otherwise wrongly promise.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UiCustomState {
    /// No override entry: this agent follows the global list.
    None,
    /// An override entry exists but is disabled — its values are preserved and
    /// will be restored if Custom is switched back on.
    PreservedDisabled,
    /// Custom mode is active; `overrides` is what this agent actually gets.
    Enabled,
}

#[derive(Serialize, Clone, Debug)]
pub struct UiAgent {
    pub name: String,
    pub version: Option<String>,
    /// RFC3339. Empty for a denied identity that was never recorded as seen.
    pub first_seen: Option<String>,
    /// RFC3339. `None` for a pre-1.3.0 record or one never seen since.
    pub last_seen: Option<String>,
    /// A live, initialized session exists for this identity right now.
    pub connected: bool,
    /// Seconds since this identity's most recent live activity, when connected.
    pub idle_secs: Option<u64>,
    pub custom_state: UiCustomState,
    /// This agent's effective per-jack values, in global jack order. Rendered
    /// dimmed when `custom_state != Enabled`.
    pub overrides: Vec<UiAgentJack>,
    pub denied: bool,
}

#[derive(Serialize, Clone, Debug)]
pub struct UiAgentJack {
    pub jack: String,
    pub on: bool,
}

#[derive(Serialize, Clone, Debug)]
pub struct UiSettings {
    /// `tray` | `window` | `both`.
    pub ui_mode: String,
    pub autostart: bool,
    pub require_approval: bool,
    pub request_logging: bool,
    pub port: u16,
    pub version: String,
}

// ---------------------------------------------------------------------------
// Snapshot construction
// ---------------------------------------------------------------------------

/// Does this jack name deserve an inline confirmation before being switched ON?
///
/// `BRIEF.md` exists because a production-database MCP must not be silently
/// reachable; the shipped `prod` jack is unpatched by default for exactly that
/// reason. A name-based heuristic is crude, and deliberately so: it is a
/// speed-bump in front of the one irreversible-feeling action, not a security
/// boundary (the real boundary is that an unpatched jack's credentials never
/// leave the gateway). Matching on the substring keeps `prod`, `eUnifyMCP-Prod`
/// and `prod-replica` all covered without a configuration knob nobody would set.
pub fn is_sensitive_jack_name(name: &str) -> bool {
    name.to_ascii_lowercase().contains("prod")
}

/// Split a `JackSummary::status` into a display status and an error reason.
fn split_status(status: &str) -> (String, Option<String>) {
    match status.strip_prefix("failed: ") {
        Some(reason) => ("failed".to_string(), Some(reason.to_string())),
        None => (status.to_string(), None),
    }
}

/// Build the whole snapshot from live state.
///
/// Cheap enough to run on every change: it takes short read locks, clones small
/// strings, and never touches the disk or an upstream process.
pub fn build_snapshot(state: &AppState, version: &str) -> UiSnapshot {
    let live = state.sessions.snapshot();
    let session_count = live.len();

    let (status, error, port) = {
        let cfg_port = state.config.read().port;
        match &*state.status.read() {
            crate::app_state::GatewayStatus::Starting => {
                ("starting".to_string(), None, cfg_port)
            }
            crate::app_state::GatewayStatus::Running { port } => {
                ("running".to_string(), None, *port)
            }
            crate::app_state::GatewayStatus::Failed { reason } => {
                ("failed".to_string(), Some(reason.clone()), cfg_port)
            }
            crate::app_state::GatewayStatus::Stopped => ("stopped".to_string(), None, cfg_port),
        }
    };

    let gateway = UiGateway {
        status,
        error,
        port,
        url: format!("http://127.0.0.1:{}/mcp", port),
        session_count,
        config_error: state.config_error.read().clone(),
    };

    let jacks: Vec<UiJack> = state
        .list_jacks()
        .into_iter()
        .map(|j| {
            let (status, error) = split_status(&j.status);
            let running = status == "running";
            UiJack {
                sensitive: is_sensitive_jack_name(&j.name),
                name: j.name,
                patched: j.patched,
                transport: j.transport,
                // Only a RUNNING jack has a meaningful count; anything else
                // reports "unknown" rather than a zero the user would read as
                // "this server has no tools".
                tool_count: if running { Some(j.tool_count) } else { None },
                status,
                error,
            }
        })
        .collect();

    let cfg = state.config.read();
    let jack_names: Vec<String> = cfg.jacks.iter().map(|j| j.name.clone()).collect();

    // One identity may hold several live sessions (an agent that reconnected
    // without closing the old one). Keep the freshest.
    let mut idle_by_name: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    for s in &live {
        if let Some(name) = &s.client_name {
            let secs = s.idle.as_secs();
            idle_by_name
                .entry(name.clone())
                .and_modify(|e| *e = (*e).min(secs))
                .or_insert(secs);
        }
    }

    let mut agents: Vec<UiAgent> = cfg
        .seen_clients
        .iter()
        .map(|c| {
            let custom_state = match cfg.client_overrides.get(&c.name) {
                None => UiCustomState::None,
                Some(o) if o.enabled => UiCustomState::Enabled,
                Some(_) => UiCustomState::PreservedDisabled,
            };
            let overrides = jack_names
                .iter()
                .map(|jack| UiAgentJack {
                    jack: jack.clone(),
                    // For an Enabled or PreservedDisabled entry this reads the
                    // agent's OWN stored value where one exists; for None it is
                    // the global value. Either way it is what switching Custom
                    // on would actually produce — the correctness plan v1 lost.
                    on: match cfg.client_overrides.get(&c.name) {
                        Some(o) => o
                            .jacks
                            .get(jack)
                            .copied()
                            .unwrap_or_else(|| cfg.effective_patched(jack, None)),
                        None => cfg.effective_patched(jack, None),
                    },
                })
                .collect();
            let idle = idle_by_name.get(&c.name).copied();
            UiAgent {
                name: c.name.clone(),
                version: c.first_seen_version.clone(),
                first_seen: if c.first_seen.is_empty() {
                    None
                } else {
                    Some(c.first_seen.clone())
                },
                last_seen: c.last_seen.clone(),
                connected: idle.is_some(),
                idle_secs: idle,
                custom_state,
                overrides,
                denied: cfg.forbidden_clients.iter().any(|f| *f == c.name),
            }
        })
        .collect();

    // Denied identities that were NEVER recorded as seen still have to appear,
    // or the user cannot un-deny them. `apply_approval_decision` writes a Deny
    // to `forbidden_clients` and NEVER to `seen_clients`, so such an identity
    // has no record and no `first_seen` — plan v1's mock showed it a date that
    // cannot exist. They render with no history at all.
    for denied in cfg.forbidden_clients.iter() {
        if !agents.iter().any(|a| a.name == *denied) {
            agents.push(UiAgent {
                name: denied.clone(),
                version: None,
                first_seen: None,
                last_seen: None,
                connected: false,
                idle_secs: None,
                custom_state: UiCustomState::None,
                overrides: Vec::new(),
                denied: true,
            });
        }
    }

    let settings = UiSettings {
        ui_mode: cfg.ui_mode.as_str().to_string(),
        autostart: cfg.autostart,
        require_approval: cfg.require_approval_for_new_clients,
        request_logging: cfg.request_logging_enabled,
        port: cfg.port,
        version: version.to_string(),
    };

    UiSnapshot {
        gateway,
        jacks,
        agents,
        settings,
    }
}

/// Build a snapshot from an `AppHandle`, taking the version from the running
/// package so it can never drift from the binary.
pub fn snapshot_for(app: &AppHandle) -> Option<UiSnapshot> {
    let state = app.try_state::<AppState>()?;
    Some(build_snapshot(&state, &app.package_info().version.to_string()))
}

/// Fetch the shared state, or a message the UI can show. Every command starts
/// here, so a missing state is reported once, in one shape.
fn state_of(app: &AppHandle) -> Result<AppState, String> {
    app.try_state::<AppState>()
        .map(|s| s.inner().clone())
        .ok_or_else(|| "Patchbay state is unavailable".to_string())
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn ui_add_jack(
    app: AppHandle,
    spec: crate::config::JackConfigInput,
) -> Result<(), String> {
    let state = state_of(&app)?;
    state.add_jack(spec).await.map(|_| ()).map_err(|e| e.message())
}

#[tauri::command]
pub async fn ui_remove_jack(app: AppHandle, name: String) -> Result<(), String> {
    let state = state_of(&app)?;
    state.remove_jack(&name).await.map_err(|e| e.message())
}

#[tauri::command]
pub async fn ui_set_client_override(
    app: AppHandle,
    agent: String,
    jack: String,
    on: bool,
) -> Result<bool, String> {
    let state = state_of(&app)?;
    state.set_client_override(&agent, &jack, on).await
}

#[tauri::command]
pub async fn ui_enable_custom(app: AppHandle, agent: String) -> Result<(), String> {
    let state = state_of(&app)?;
    state.enable_custom_client(&agent).await
}

#[tauri::command]
pub async fn ui_disable_custom(app: AppHandle, agent: String) -> Result<(), String> {
    let state = state_of(&app)?;
    state.disable_custom_client(&agent).await
}

/// Discard an agent's preserved Custom list (plan §3.3). The one operation the
/// tray cannot express.
#[tauri::command]
pub async fn ui_reset_custom_to_global(app: AppHandle, agent: String) -> Result<(), String> {
    let state = state_of(&app)?;
    state.reset_custom_to_global(&agent).await
}

#[tauri::command]
pub async fn ui_set_forbidden(app: AppHandle, agent: String, denied: bool) -> Result<(), String> {
    let state = state_of(&app)?;
    state.set_forbidden(&agent, denied).await
}

/// Delete agent identities, returning the undo token the strip holds (W-D12).
#[tauri::command]
pub async fn ui_delete_agents(app: AppHandle, names: Vec<String>) -> Result<u64, String> {
    let state = state_of(&app)?;
    Ok(state.delete_agents(&names).await)
}

#[tauri::command]
pub async fn ui_undo(app: AppHandle, token: u64) -> Result<usize, String> {
    let state = state_of(&app)?;
    state.undo_delete_agents(token).await
}

/// Deny or allow several identities at once. One backend call, because N
/// concurrent single calls raced each other to disk and lost all but one.
#[tauri::command]
pub async fn ui_set_forbidden_batch(
    app: AppHandle,
    agents: Vec<String>,
    denied: bool,
) -> Result<usize, String> {
    let state = state_of(&app)?;
    state.set_forbidden_batch(&agents, denied).await
}

#[tauri::command]
pub fn ui_set_autostart(app: AppHandle, on: bool) -> Result<(), String> {
    let state = state_of(&app)?;
    state.set_autostart(on)
}

#[tauri::command]
pub fn ui_set_require_approval(app: AppHandle, on: bool) -> Result<(), String> {
    let state = state_of(&app)?;
    state.set_require_approval(on)
}

#[tauri::command]
pub fn ui_set_request_logging(app: AppHandle, on: bool) -> Result<(), String> {
    let state = state_of(&app)?;
    state.set_request_logging_enabled(on)
}

/// Reload `patchbay.json` from disk, exactly as the tray item does.
#[tauri::command]
pub fn ui_reload_config(app: AppHandle) {
    crate::tray::on_reload(&app);
}

#[tauri::command]
pub fn ui_retry_gateway(app: AppHandle) {
    crate::tray::on_retry_gateway(&app);
}

#[tauri::command]
pub fn ui_open_config(app: AppHandle) {
    crate::tray::on_open_config(&app);
}

#[tauri::command]
pub fn ui_open_logs(app: AppHandle) {
    crate::tray::on_open_logs(&app);
}

#[tauri::command]
pub async fn ui_set_port(app: AppHandle, port: u16) -> Result<(), String> {
    let state = state_of(&app)?;
    state.set_port(port).await
}

#[tauri::command]
pub fn ui_copy_url(app: AppHandle) {
    crate::tray::on_copy_url(&app);
}

#[tauri::command]
pub fn ui_about(app: AppHandle) {
    crate::tray::on_about(&app);
}

#[tauri::command]
pub fn ui_snapshot(app: AppHandle) -> Option<UiSnapshot> {
    snapshot_for(&app)
}

#[tauri::command]
pub async fn ui_toggle_jack(app: AppHandle, name: String, on: bool) -> Option<ToggleResult> {
    let state = app.try_state::<AppState>()?.inner().clone();
    // No fan-out here: `set_patched` does it itself (W-D4). Calling it again
    // would push two identical snapshots for one click.
    let result = state.set_patched(&name, on).await;
    Some(result)
}

#[tauri::command]
pub fn ui_set_ui_mode(app: AppHandle, mode: String) -> Result<(), String> {
    let state = state_of(&app)?;
    let mode = UiMode::from_str_lenient(&mode);
    state.set_ui_mode(mode)?;
    crate::tray::apply_ui_mode(&app, mode);
    state.notify_state_changed();
    Ok(())
}

#[tauri::command]
pub fn ui_close_window(app: AppHandle) {
    crate::window::hide_popover(&app);
}

#[tauri::command]
pub fn ui_quit(app: AppHandle) {
    log("ui: quit requested from the window");
    app.exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{first_run_template, JackTransport};
    use std::collections::BTreeMap;

    #[test]
    fn snapshot_never_leaks_secrets() {
        // The whole trust story of the window rests on this: a page that renders
        // attacker-influenced strings must never have been handed a credential
        // in the first place.
        let mut cfg = first_run_template();
        cfg.jacks[0].transport = JackTransport::StreamableHttp {
            url: "https://example.com/mcp?token=URLSECRET".to_string(),
            headers: BTreeMap::from([(
                "Authorization".to_string(),
                "Bearer HEADERSECRET".to_string(),
            )]),
        };
        cfg.jacks.push(crate::config::JackConfig {
            name: "child".to_string(),
            patched: false,
            transport: JackTransport::Stdio {
                command: "npx".to_string(),
                args: vec!["-y".to_string(), "some-mcp".to_string()],
                env: BTreeMap::from([("DB_TOKEN".to_string(), "ENVSECRET".to_string())]),
            },
            sharing: crate::config::Sharing::Shared,
            tools: None,
        });

        let state = AppState::new(cfg);
        let snap = build_snapshot(&state, "1.3.0");
        let json = serde_json::to_string(&snap).expect("serialize");

        for secret in ["HEADERSECRET", "ENVSECRET", "URLSECRET", "Bearer", "dpapi:"] {
            assert!(
                !json.contains(secret),
                "snapshot leaked '{}': {}",
                secret,
                json
            );
        }
        // Sanity: the snapshot is not empty (a test that passes because nothing
        // was serialized would be worthless).
        assert!(json.contains("child"), "jack names must still be present");
    }

    #[test]
    fn sensitive_jacks_are_flagged() {
        assert!(is_sensitive_jack_name("prod"));
        assert!(is_sensitive_jack_name("eUnifyMCP-Prod"));
        assert!(is_sensitive_jack_name("prod-replica"));
        assert!(!is_sensitive_jack_name("bee-memory-bank"));
        assert!(!is_sensitive_jack_name("tabduct"));
    }

    #[test]
    fn tool_count_is_none_unless_running() {
        // "0 tools" on a stopped jack reads as "this server is empty"; the UI
        // must be able to say "off" instead.
        let state = AppState::new(first_run_template());
        let snap = build_snapshot(&state, "1.3.0");
        assert_eq!(snap.jacks[0].tool_count, None);
        assert_eq!(snap.jacks[0].status, "unknown");
    }

    #[test]
    fn status_failure_reason_is_split_out() {
        let (s, e) = split_status("failed: ECONNREFUSED");
        assert_eq!(s, "failed");
        assert_eq!(e.as_deref(), Some("ECONNREFUSED"));
        let (s, e) = split_status("running");
        assert_eq!(s, "running");
        assert_eq!(e, None);
    }

    #[test]
    fn custom_state_distinguishes_preserved_from_absent() {
        let mut cfg = first_run_template();
        cfg.seen_clients.push(crate::config::SeenClient {
            name: "plain".to_string(),
            first_seen_version: None,
            first_seen: "2026-07-01T00:00:00+02:00".to_string(),
            last_seen: None,
        });
        cfg.seen_clients.push(crate::config::SeenClient {
            name: "preserved".to_string(),
            first_seen_version: None,
            first_seen: "2026-07-01T00:00:00+02:00".to_string(),
            last_seen: None,
        });
        cfg.seen_clients.push(crate::config::SeenClient {
            name: "active".to_string(),
            first_seen_version: None,
            first_seen: "2026-07-01T00:00:00+02:00".to_string(),
            last_seen: None,
        });
        // A DISABLED override that still carries a value differing from global.
        cfg.client_overrides.insert(
            "preserved".to_string(),
            crate::config::ClientOverride {
                enabled: false,
                jacks: BTreeMap::from([("prod".to_string(), true)]),
            },
        );
        cfg.client_overrides.insert(
            "active".to_string(),
            crate::config::ClientOverride {
                enabled: true,
                jacks: BTreeMap::from([("prod".to_string(), true)]),
            },
        );

        let state = AppState::new(cfg);
        let snap = build_snapshot(&state, "1.3.0");
        let by = |n: &str| snap.agents.iter().find(|a| a.name == n).unwrap().clone();

        assert_eq!(by("plain").custom_state, UiCustomState::None);
        assert_eq!(
            by("preserved").custom_state,
            UiCustomState::PreservedDisabled
        );
        assert_eq!(by("active").custom_state, UiCustomState::Enabled);

        // The preserved agent must advertise its PRESERVED value (prod ON),
        // not the global one (prod OFF) — showing the global value here is
        // exactly the lie plan v1 would have told: the switch says one thing,
        // enabling Custom produces another.
        let preserved_prod = by("preserved")
            .overrides
            .iter()
            .find(|o| o.jack == "prod")
            .unwrap()
            .on;
        assert!(
            preserved_prod,
            "a preserved override must be shown as it will be restored"
        );
        let plain_prod = by("plain")
            .overrides
            .iter()
            .find(|o| o.jack == "prod")
            .unwrap()
            .on;
        assert!(!plain_prod, "an agent with no entry follows the global list");
    }

    #[test]
    fn denied_identity_without_a_seen_record_still_appears() {
        // A Deny writes to forbidden_clients and NEVER to seen_clients. If the
        // UI listed only seen_clients, a denied agent would be invisible and
        // therefore impossible to un-deny.
        let mut cfg = first_run_template();
        cfg.forbidden_clients.push("rogue".to_string());
        let state = AppState::new(cfg);
        let snap = build_snapshot(&state, "1.3.0");

        let rogue = snap
            .agents
            .iter()
            .find(|a| a.name == "rogue")
            .expect("a denied identity must be listed");
        assert!(rogue.denied);
        assert_eq!(
            rogue.first_seen, None,
            "it has no seen_clients record, so it has no first_seen date"
        );
        assert!(!rogue.connected);
    }

    // ---- (S13 W-D11 layer 2) the frontend may not build markup from strings --

    /// The popover renders strings chosen by whoever connects to the gateway.
    /// Preact escapes text by construction — but only for as long as nobody
    /// reaches for a raw-HTML escape hatch. This check makes that a build
    /// failure rather than a code-review hope.
    #[test]
    fn frontend_never_uses_raw_html_sinks() {
        const APP_JS: &str = include_str!("../../dist/app.js");
        const INDEX: &str = include_str!("../../dist/index.html");

        for (label, source) in [("dist/app.js", APP_JS), ("dist/index.html", INDEX)] {
            for sink in [
                "innerHTML",
                "outerHTML",
                "insertAdjacentHTML",
                "dangerouslySetInnerHTML",
                "document.write",
                "eval(",
                "new Function",
            ] {
                assert!(
                    !source.contains(sink),
                    "{} uses '{}': untrusted agent names are rendered by this page, so a raw-HTML \
                     sink turns a display string into script execution (WINDOW_UI_PLAN W-D11).",
                    label,
                    sink
                );
            }
        }
    }

    /// The vendored libraries are the other half of that promise: they run
    /// under the same strict CSP and must not need an eval relaxation.
    #[test]
    fn vendored_libraries_contain_no_eval() {
        for (name, source) in [
            ("preact", include_str!("../../dist/vendor/preact.umd.js")),
            ("hooks", include_str!("../../dist/vendor/hooks.umd.js")),
            ("htm", include_str!("../../dist/vendor/htm.umd.js")),
        ] {
            assert!(
                !source.contains("eval(") && !source.contains("new Function"),
                "vendored {} needs eval, which would force loosening script-src",
                name
            );
        }
    }

    /// Every command the page calls must actually be registered, or the button
    /// fails silently at runtime with an unhelpful IPC error.
    #[test]
    fn every_command_the_frontend_calls_is_registered() {
        const APP_JS: &str = include_str!("../../dist/app.js");
        const MAIN_RS: &str = include_str!("main.rs");

        let mut missing = Vec::new();
        let mut rest = APP_JS;
        while let Some(i) = rest.find("invoke(\"") {
            rest = &rest[i + 8..];
            let end = match rest.find('"') {
                Some(e) => e,
                None => break,
            };
            let name = &rest[..end];
            if !MAIN_RS.contains(&format!("ui::{},", name)) {
                missing.push(name.to_string());
            }
            rest = &rest[end..];
        }
        assert!(
            missing.is_empty(),
            "the popover calls commands that are not in generate_handler!: {:?}",
            missing
        );
    }

    // ---- (S13 W6) colour contrast, both themes ---------------------------

    /// Relative luminance per WCAG 2.1.
    fn luminance(hex: &str) -> f64 {
        fn channel(c: f64) -> f64 {
            if c <= 0.03928 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        }
        let h = hex.trim().trim_start_matches('#');
        let v = |i: usize| {
            u8::from_str_radix(&h[i..i + 2], 16).expect("hex colour") as f64 / 255.0
        };
        0.2126 * channel(v(0)) + 0.7152 * channel(v(2)) + 0.0722 * channel(v(4))
    }

    fn contrast(a: &str, b: &str) -> f64 {
        let (la, lb) = (luminance(a), luminance(b));
        let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
        (hi + 0.05) / (lo + 0.05)
    }

    /// Pull `--token: value;` pairs out of one CSS block.
    fn tokens_in(block: &str) -> std::collections::HashMap<String, String> {
        let mut out = std::collections::HashMap::new();
        for line in block.lines() {
            let line = line.trim();
            if !line.starts_with("--") {
                continue;
            }
            if let Some((name, value)) = line.split_once(':') {
                let value = value.trim().trim_end_matches(';').trim();
                if value.starts_with('#') {
                    out.insert(name.trim().to_string(), value.to_string());
                }
            }
        }
        out
    }

    /// Every foreground/background pair the popover actually renders must meet
    /// its WCAG minimum — 4.5:1 for text, 3:1 for the boundary of a control —
    /// in BOTH themes.
    ///
    /// Read out of the real stylesheet rather than restated here, so the check
    /// cannot drift from what ships. It found a genuine failure when first run:
    /// `--stroke-strong` was 1.67:1, making the border of every text input and
    /// secondary button effectively invisible.
    #[test]
    fn colour_contrast_meets_wcag_in_both_themes() {
        const CSS: &str = include_str!("../../dist/app.css");

        // The first `:root {` block is the light theme; the one inside the
        // prefers-color-scheme query overrides it for dark.
        let mut blocks = Vec::new();
        let mut rest = CSS;
        while let Some(i) = rest.find(":root") {
            rest = &rest[i..];
            let open = match rest.find('{') {
                Some(o) => o,
                None => break,
            };
            let close = match rest[open..].find('}') {
                Some(c) => open + c,
                None => break,
            };
            blocks.push(&rest[open + 1..close]);
            rest = &rest[close..];
        }
        assert!(blocks.len() >= 2, "expected a light and a dark :root block");

        let light = tokens_in(blocks[0]);
        let mut dark = light.clone();
        for (k, v) in tokens_in(blocks[1]) {
            dark.insert(k, v);
        }

        // (foreground, background, minimum, what the user sees)
        const PAIRS: &[(&str, &str, f64, &str)] = &[
            ("--text", "--card", 4.5, "row name"),
            ("--text", "--bg", 4.5, "body text"),
            ("--text-dim", "--card", 4.5, "row meta line"),
            ("--text-dim", "--bg", 4.5, "section heading, footer link"),
            ("--accent-text", "--accent", 4.5, "label on an accent button"),
            ("--err", "--card", 4.5, "failure text on a row"),
            ("--err", "--bg", 4.5, "failure text on the ground"),
            ("--ok", "--card", 3.0, "running dot"),
            ("--ok-text", "--ok", 4.5, "knob on the green master switch"),
            ("--warn", "--card", 3.0, "starting dot, confirm border"),
            ("--accent", "--card", 3.0, "switch fill, focus ring"),
            ("--accent", "--bg", 3.0, "focus ring on the ground"),
            ("--switch-off", "--card", 3.0, "off switch outline"),
            ("--stroke-strong", "--card", 3.0, "input and button border"),
        ];

        let mut failures = Vec::new();
        for (theme, map) in [("light", &light), ("dark", &dark)] {
            for (fg, bg, need, what) in PAIRS {
                let f = map.get(*fg).unwrap_or_else(|| panic!("missing {}", fg));
                let b = map.get(*bg).unwrap_or_else(|| panic!("missing {}", bg));
                let r = contrast(f, b);
                if r < *need {
                    failures.push(format!(
                        "{}: {} on {} ({}) is {:.2}:1, needs {:.1}:1",
                        theme, fg, bg, what, r, need
                    ));
                }
            }
        }
        assert!(failures.is_empty(), "contrast failures:\n{}", failures.join("\n"));
    }
}
