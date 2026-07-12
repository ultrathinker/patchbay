//! Typed, validated configuration schema for Patchbay.
//!
//! Matches the JSON shape in `_planning/MASTER_PLAN.md` ("Config schema").
//! The transport-specific fields (`command`/`args`/`env` for stdio,
//! `url`/`headers` for streamable-http) are flattened to the top level of each
//! jack object via an internally-tagged enum (`#[serde(tag = "transport")]`)
//! combined with `#[serde(flatten)]` on the `transport` field of `JackConfig`.
//! That yields exactly:
//! ```jsonc
//! { "name": "prod", "patched": false, "transport": "stdio",
//!   "command": "npx", "args": ["-y","some-db-mcp"], "env": {...},
//!   "sharing": "shared", "tools": null }
//! ```
//!
//! Serde defaults are applied liberally so old/partial configs keep loading
//! (e.g. a jack missing `args`/`env`/`sharing`/`patched`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Default gateway port (MASTER_PLAN: bind strictly `127.0.0.1:{port}`).
pub const DEFAULT_PORT: u16 = 39100;

/// Current on-disk config schema version.
pub const CURRENT_VERSION: u32 = 1;

fn default_version() -> u32 {
    CURRENT_VERSION
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

/// Top-level Patchbay configuration (the whole `patchbay.json` file).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PatchbayConfig {
    /// Schema version. Bumped on breaking changes.
    #[serde(default = "default_version")]
    pub version: u32,

    /// Gateway port, bound to `127.0.0.1` only.
    #[serde(default = "default_port")]
    pub port: u16,

    /// Start Patchbay with Windows (HKCU `Run\Patchbay`).
    #[serde(default)]
    pub autostart: bool,

    /// The MCP servers Patchbay fronts. Order is preserved for tray display.
    #[serde(default)]
    pub jacks: Vec<JackConfig>,

    /// Reserved for presets / future groupings (non-goal in v0.1). Opaque.
    #[serde(default)]
    pub bays: BTreeMap<String, serde_json::Value>,

    /// MCP client names (`clientInfo.name`) that have connected at least once.
    /// Append-only: a name already present is never duplicated or rewritten.
    /// (S10) Powers the tray "Custom" submenu so a newly-seen agent can be
    /// customized without a manual "Reload config".
    #[serde(default)]
    pub seen_clients: Vec<SeenClient>,

    /// Per-client custom on/off lists (S10). Keyed by the RAW `clientInfo.name`
    /// (no normalization / case-fold, so a renamed client shows up as a distinct
    /// new entry instead of silently inheriting). Presence of an entry with
    /// `enabled: true` is what makes a client "Custom": it then gets its OWN
    /// full list mirroring every global jack (same names; only the per-jack bool
    /// values can differ). An absent entry, or one with `enabled: false`, means
    /// the client inherits the global list.
    #[serde(default)]
    pub client_overrides: BTreeMap<String, ClientOverride>,

    /// Whether a NEW (never-seen) client identity must be approved via a native
    /// dialog before it gets any tools (S10c). Default OFF (plain
    /// `#[serde(default)]`, `bool`'s `Default` is `false`): unknown clients
    /// connect straight through and are auto-added to `seen_clients`
    /// immediately, no dialog, no waiting on the user. The user can flip this
    /// on from the tray ("Require approval for new agents") if they want the
    /// gate. A client already in `seen_clients` or `forbidden_clients` never
    /// re-triggers the dialog regardless of this setting (a previous decision
    /// stands).
    #[serde(default)]
    pub require_approval_for_new_clients: bool,

    /// Whether the Level-2 request/event log is on (off by default, per the
    /// explicit user requirement). When `true`, the gateway writes a line per
    /// handled MCP request (`[REQUEST]`, with redacted headers + truncated
    /// body) and every admin/lifecycle action (`[EVENT]` / `[ERROR]`) to
    /// `%APPDATA%\Patchbay\logs\requests\<YYYY-MM-DD>.log`. Checked live on every
    /// write, so toggling it from the tray takes effect immediately without an
    /// app restart. Plain `#[serde(default)]` (`bool`'s `Default` is `false`).
    #[serde(default)]
    pub request_logging_enabled: bool,

    /// Client identities the user has DENIED at the first-connection gate (S10c).
    /// A forbidden client's `initialize` still completes (so the agent sees no
    /// confusing transport-level failure), but `tools/list` returns ZERO tools
    /// (no jack tools AND no gateway-owned `patchbay__*` meta tools) and any
    /// `tools/call` returns the D2-style `CallToolResult{is_error:true}` with a
    /// "blocked by the user" message. Append-only via the approval dialog; a
    /// tray "Forbidden (N)" submenu lists them and lets the user un-forbid one
    /// (which does NOT retroactively grant `seen_clients` status — the client
    /// goes through the gate again on its next connect).
    #[serde(default)]
    pub forbidden_clients: Vec<String>,
}

/// A client (MCP agent) Patchbay has seen connect at least once (S10). Append-
/// only metadata surfaced in the tray "Custom" submenu.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SeenClient {
    /// The raw `clientInfo.name` from the `initialize` request.
    pub name: String,
    /// `clientInfo.version` from the first sighting (display only). `None` when
    /// the client omitted it.
    #[serde(default)]
    pub first_seen_version: Option<String>,
    /// RFC3339 timestamp of the first sighting (display only).
    #[serde(default)]
    pub first_seen: String,
}

/// One client's custom on/off list (S10). When `enabled`, the client uses
/// `jacks` instead of the global list; otherwise it inherits the global list.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ClientOverride {
    /// `true` = this client is in "Custom" mode (uses `jacks`).
    #[serde(default)]
    pub enabled: bool,
    /// Per-jack on/off for THIS client. Always mirrors the SAME set of jack
    /// NAMES as the global list (kept in sync on add/remove); only the bool
    /// values differ.
    #[serde(default)]
    pub jacks: BTreeMap<String, bool>,
}

/// A single MCP server ("jack") Patchbay can patch in/out.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct JackConfig {
    /// Human + machine name. Validated: `^[A-Za-z0-9_-]+$`, no `__`, <=40,
    /// unique. Used as the namespace prefix (`<name>__<tool>`).
    pub name: String,

    /// ON (true) / OFF (false). `prod` ships `false` (MASTER_PLAN D4).
    #[serde(default)]
    pub patched: bool,

    /// Transport + its fields, flattened to the jack's top level so the
    /// `transport` discriminator sits beside `command`/`url` etc.
    #[serde(flatten)]
    pub transport: JackTransport,

    /// Sharing model for the upstream child/connection.
    /// v0.1 only implements `Shared`; `PerClientSession` is additive later.
    #[serde(default)]
    pub sharing: Sharing,

    /// Reserved for v0.2 allow/deny tool lists. Opaque so any future shape
    /// round-trips; `null` when absent.
    #[serde(default)]
    pub tools: Option<serde_json::Value>,
}

/// How a jack talks to its MCP server.
///
/// `#[serde(tag = "transport", rename_all = "kebab-case")]` produces the
/// discriminator values `"stdio"` and `"streamable-http"` from the variant
/// names, matching the documented schema.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "transport", rename_all = "kebab-case")]
pub enum JackTransport {
    /// Spawn a local process; speak newline-delimited JSON-RPC over its
    /// stdio. One shared child per patched jack in v0.1.
    Stdio {
        #[serde(default)]
        command: String,
        #[serde(default)]
        args: Vec<String>,
        /// Environment variables. Values may be `dpapi:`-encrypted or plain;
        /// plaintext is wrapped on save (see `config::secrets`).
        #[serde(default)]
        env: BTreeMap<String, String>,
    },
    /// Connect to a remote Streamable HTTP MCP server.
    StreamableHttp {
        #[serde(default)]
        url: String,
        /// Request headers (e.g. `Authorization`). Same dpapi/plain rules as env.
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
}

/// Upstream sharing model. Renamed to `"shared"` / `"per_client_session"`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Sharing {
    /// One shared child/connection serves every agent session (v0.1 default).
    #[default]
    Shared,
    /// One child/connection per client session (additive, post-v0.1).
    PerClientSession,
}

/// Input shape for adding a jack via the admin API / meta MCP tools (S8).
///
/// Mirrors [`JackConfig`] closely enough that an admin-created jack round-trips
/// through `patchbay.json` IDENTICALLY to a hand-written one: `transport` is
/// flattened with the SAME internally-tagged discriminator
/// (`"transport": "stdio"|"streamable-http"`, kebab-case) and the SAME variant
/// field names (`command`/`args`/`env` for stdio, `url`/`headers` for
/// streamable-http) as [`JackTransport`]. So the JSON an agent POSTs is the same
/// shape as a jack object already sitting in the config file.
///
/// `patched` defaults to `true` (an added jack is ON and auto-started unless the
/// caller says otherwise). `tools` is reserved (v0.2) and not exposed here; it
/// defaults to `None` when the input is turned into a [`JackConfig`].
///
/// Example:
/// ```jsonc
/// { "name": "docs", "patched": true, "transport": "streamable-http",
///   "url": "https://example.com/mcp", "headers": { "Authorization": "Bearer x" } }
/// ```
#[derive(Deserialize, Clone, Debug)]
pub struct JackConfigInput {
    /// Human + machine name (validated via [`crate::config::is_valid_jack_name`]).
    pub name: String,

    /// ON (true, default) / OFF (false).
    #[serde(default = "default_patched_true")]
    pub patched: bool,

    /// Transport + its fields, flattened to the input's top level (same shape as
    /// [`JackConfig::transport`]).
    #[serde(flatten)]
    pub transport: JackTransport,

    /// Sharing model (defaults to `Shared`, like [`JackConfig`]).
    #[serde(default)]
    pub sharing: Sharing,
}

/// `#[serde(default)]` helper for [`JackConfigInput::patched`] (S8): an added
/// jack is patched ON unless the caller explicitly sets `patched: false`.
fn default_patched_true() -> bool {
    true
}

// ---- per-client resolution (S10) ----------------------------------------

impl PatchbayConfig {
    /// The effective `patched` value of a jack for a given connecting client
    /// (S10). This is the ENFORCEMENT side (what a client sees in tools/list and
    /// whether tools/call is allowed for it) — it is independent of whether the
    /// shared child PROCESS is running (see [`Self::should_run_jack`]).
    ///
    /// - (S10c) If the client is FORBIDDEN ([`Self::is_forbidden`]), always
    ///   `false`: a forbidden agent sees no jack tools and every call is blocked.
    /// - If `client_name` is `Some`, an override exists for it with
    ///   `enabled: true`, AND its `jacks` map has an entry for `jack_name`,
    ///   return that per-client value.
    /// - Otherwise fall back to the jack's own GLOBAL `patched` (defensive: a
    ///   Custom client whose list is somehow missing an entry — e.g. a past bug
    ///   — must NOT silently deny/allow incorrectly; it inherits the global
    ///   value rather than panicking or default-denying).
    pub fn effective_patched(&self, jack_name: &str, client_name: Option<&str>) -> bool {
        // (S10c) Forbidden gate: highest-priority enforcement — a blocked agent
        // never sees a jack as patched, even if a Custom override or the global
        // flag would otherwise say yes.
        if self.is_forbidden(client_name) {
            return false;
        }
        if let Some(name) = client_name {
            if let Some(ovr) = self.client_overrides.get(name) {
                if ovr.enabled {
                    if let Some(&per_client) = ovr.jacks.get(jack_name) {
                        return per_client;
                    }
                }
            }
        }
        self.jacks
            .iter()
            .find(|j| j.name == jack_name)
            .map(|j| j.patched)
            .unwrap_or(false)
    }

    /// Whether a client identity has been DENIED at the first-connection gate
    /// (S10c). `None` (no identity) is never forbidden (an unidentified client
    /// falls through to the global list only). Exact-string match — no
    /// normalization, consistent with `client_overrides`/`seen_clients` keying.
    pub fn is_forbidden(&self, client_name: Option<&str>) -> bool {
        match client_name {
            Some(name) => self.forbidden_clients.iter().any(|c| c == name),
            None => false,
        }
    }

    /// Whether the SHARED upstream child process for a jack should be running,
    /// considering EVERY known consumer (S10). This is the PROCESS-LIFECYCLE
    /// side — it only controls whether the shared child is alive, NOT whether a
    /// specific client can see/call the jack (that is `effective_patched`).
    ///
    /// A stdio jack runs ONE shared child for ALL clients. A jack that is OFF
    /// globally but ON for one Custom client must still have its child alive so
    /// that client can use it; conversely a jack ON globally keeps its child
    /// alive even if no Custom client lists it. `should_run` is the OR over all
    /// of these.
    ///
    /// `should_run(jack) = jack.patched (global) OR
    ///   (exists non-forbidden enabled client_overrides[*] whose
    ///    jacks[jack] == Some(true))`
    pub fn should_run_jack(&self, jack_name: &str) -> bool {
        if self
            .jacks
            .iter()
            .find(|j| j.name == jack_name)
            .map(|j| j.patched)
            .unwrap_or(false)
        {
            return true;
        }
        for (client, ovr) in &self.client_overrides {
            if self.is_forbidden(Some(client)) {
                continue;
            }
            if ovr.enabled && ovr.jacks.get(jack_name) == Some(&true) {
                return true;
            }
        }
        false
    }

    /// Number of clients currently in "Custom" mode (S10): `client_overrides`
    /// entries with `enabled: true`, excluding forbidden identities because the
    /// forbidden gate takes precedence over Custom. Surfaced as the "Custom (N)"
    /// tray label.
    pub fn custom_client_count(&self) -> usize {
        self.client_overrides
            .iter()
            .filter(|(client, o)| o.enabled && !self.is_forbidden(Some(client)))
            .count()
    }
}

// ---- accessors -----------------------------------------------------------

impl JackTransport {
    /// Mutable env map for stdio jacks (no-op for http).
    pub fn env_mut(&mut self) -> Option<&mut BTreeMap<String, String>> {
        match self {
            JackTransport::Stdio { env, .. } => Some(env),
            _ => None,
        }
    }

    /// Mutable headers map for streamable-http jacks (no-op for stdio).
    pub fn headers_mut(&mut self) -> Option<&mut BTreeMap<String, String>> {
        match self {
            JackTransport::StreamableHttp { headers, .. } => Some(headers),
            _ => None,
        }
    }

    /// Shared env map for stdio jacks (empty for http).
    pub fn env(&self) -> Option<&BTreeMap<String, String>> {
        match self {
            JackTransport::Stdio { env, .. } => Some(env),
            _ => None,
        }
    }

    /// Shared headers map for streamable-http jacks (empty for stdio).
    pub fn headers(&self) -> Option<&BTreeMap<String, String>> {
        match self {
            JackTransport::StreamableHttp { headers, .. } => Some(headers),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stdio_jack(name: &str) -> JackConfig {
        JackConfig {
            name: name.to_string(),
            patched: false,
            transport: JackTransport::Stdio {
                command: "npx".to_string(),
                args: vec!["-y".to_string()],
                env: BTreeMap::new(),
            },
            sharing: Sharing::Shared,
            tools: None,
        }
    }

    #[test]
    fn stdio_serializes_with_transport_tag_and_fields() {
        let j = stdio_jack("prod");
        let v: serde_json::Value = serde_json::to_value(&j).unwrap();
        // Internally-tagged + flattened: discriminator + variant fields at top level.
        assert_eq!(v["transport"], "stdio");
        assert_eq!(v["name"], "prod");
        assert_eq!(v["command"], "npx");
        assert_eq!(v["args"], serde_json::json!(["-y"]));
        assert_eq!(v["env"], serde_json::json!({}));
        assert_eq!(v["sharing"], "shared");
        assert_eq!(v["tools"], serde_json::Value::Null);
        // No stray nesting of the transport variant.
        assert!(v.get("Stdio").is_none());
    }

    #[test]
    fn http_serializes_kebab_case_tag_and_snake_case_sharing() {
        let j = JackConfig {
            name: "docs".to_string(),
            patched: true,
            transport: JackTransport::StreamableHttp {
                url: "https://example.com/mcp".to_string(),
                headers: BTreeMap::new(),
            },
            sharing: Sharing::PerClientSession,
            tools: None,
        };
        let v: serde_json::Value = serde_json::to_value(&j).unwrap();
        assert_eq!(v["transport"], "streamable-http");
        assert_eq!(v["url"], "https://example.com/mcp");
        assert_eq!(v["headers"], serde_json::json!({}));
        assert_eq!(v["sharing"], "per_client_session");
    }

    #[test]
    fn partial_config_loads_with_serde_defaults() {
        // Missing patched/args/env/sharing/tools must still load.
        let json = r#"{
            "version": 1,
            "port": 39100,
            "jacks": [
                { "name": "x", "transport": "stdio", "command": "node" }
            ]
        }"#;
        let cfg: PatchbayConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.autostart, false);
        assert_eq!(cfg.bays, BTreeMap::new());
        // (S10c) Omitted fields default: gate OFF, no forbidden clients.
        assert!(!cfg.require_approval_for_new_clients);
        assert!(cfg.forbidden_clients.is_empty());
        assert_eq!(cfg.jacks.len(), 1);
        let j = &cfg.jacks[0];
        assert_eq!(j.name, "x");
        assert_eq!(j.patched, false);
        assert_eq!(j.sharing, Sharing::Shared);
        assert_eq!(j.tools, None);
        match &j.transport {
            JackTransport::Stdio { command, args, env } => {
                assert_eq!(command, "node");
                assert!(args.is_empty());
                assert!(env.is_empty());
            }
            _ => panic!("expected stdio"),
        }
    }

    #[test]
    fn top_level_defaults_when_jacks_missing() {
        let json = r#"{"version":1,"port":39100}"#;
        let cfg: PatchbayConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.jacks.is_empty());
        assert_eq!(cfg.autostart, false);
    }

    #[test]
    fn round_trip_through_json_value() {
        let cfg = PatchbayConfig {
            version: 1,
            port: 1234,
            autostart: true,
            jacks: vec![stdio_jack("a"), stdio_jack("b")],
            bays: BTreeMap::new(),
            seen_clients: Vec::new(),
            client_overrides: BTreeMap::new(),
            require_approval_for_new_clients: true,
            request_logging_enabled: false,
            forbidden_clients: Vec::new(),
        };
        let s = serde_json::to_string(&cfg).unwrap();
        let back: PatchbayConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(cfg, back);
    }

    // ---- S10: effective_patched / should_run_jack -------------------------

    fn jack_named(name: &str, patched: bool) -> JackConfig {
        JackConfig {
            name: name.to_string(),
            patched,
            transport: JackTransport::Stdio {
                command: "npx".to_string(),
                args: vec![],
                env: BTreeMap::new(),
            },
            sharing: Sharing::Shared,
            tools: None,
        }
    }

    fn cfg_with_jacks(jacks: Vec<JackConfig>) -> PatchbayConfig {
        PatchbayConfig {
            version: CURRENT_VERSION,
            port: DEFAULT_PORT,
            autostart: false,
            jacks,
            bays: BTreeMap::new(),
            seen_clients: Vec::new(),
            client_overrides: BTreeMap::new(),
            require_approval_for_new_clients: true,
            request_logging_enabled: false,
            forbidden_clients: Vec::new(),
        }
    }

    fn make_override(enabled: bool, entries: &[(&str, bool)]) -> ClientOverride {
        let mut jacks = BTreeMap::new();
        for (n, v) in entries {
            jacks.insert(n.to_string(), *v);
        }
        ClientOverride { enabled, jacks }
    }

    #[test]
    fn effective_patched_falls_back_to_global_when_no_override() {
        // alpha ON globally, beta OFF globally.
        let cfg = cfg_with_jacks(vec![jack_named("alpha", true), jack_named("beta", false)]);
        // No client name -> global values.
        assert!(cfg.effective_patched("alpha", None));
        assert!(!cfg.effective_patched("beta", None));
        // A client name with no override entry -> global values.
        assert!(cfg.effective_patched("alpha", Some("claude-code")));
        assert!(!cfg.effective_patched("beta", Some("claude-code")));
        // Unknown jack -> false (defensive).
        assert!(!cfg.effective_patched("ghost", Some("claude-code")));
    }

    #[test]
    fn effective_patched_uses_enabled_override_entry() {
        let mut cfg = cfg_with_jacks(vec![jack_named("alpha", true), jack_named("beta", false)]);
        // Custom client turns alpha OFF (overrides global ON) and beta ON
        // (overrides global OFF).
        cfg.client_overrides.insert(
            "codex".to_string(),
            make_override(true, &[("alpha", false), ("beta", true)]),
        );
        assert!(!cfg.effective_patched("alpha", Some("codex")));
        assert!(cfg.effective_patched("beta", Some("codex")));
        // A DIFFERENT client still gets the global values.
        assert!(cfg.effective_patched("alpha", Some("other")));
    }

    #[test]
    fn effective_patched_disabled_override_falls_back_to_global() {
        let mut cfg = cfg_with_jacks(vec![jack_named("alpha", true)]);
        // Override present but disabled -> ignored, global wins.
        cfg.client_overrides
            .insert("codex".to_string(), make_override(false, &[("alpha", false)]));
        assert!(cfg.effective_patched("alpha", Some("codex")));
    }

    #[test]
    fn effective_patched_override_missing_jack_falls_back_to_global() {
        let mut cfg = cfg_with_jacks(vec![jack_named("alpha", true), jack_named("beta", true)]);
        // Enabled override lists only alpha; beta entry is absent -> defensive
        // fallback to global (true), NOT a default-deny.
        cfg.client_overrides
            .insert("codex".to_string(), make_override(true, &[("alpha", false)]));
        assert!(!cfg.effective_patched("alpha", Some("codex")));
        assert!(cfg.effective_patched("beta", Some("codex")));
    }

    #[test]
    fn should_run_jack_or_over_global_and_client_overrides() {
        let mut cfg = cfg_with_jacks(vec![jack_named("alpha", false)]);
        // Global off, no overrides -> should NOT run.
        assert!(!cfg.should_run_jack("alpha"));
        // Global off but one enabled client wants it ON -> should run.
        cfg.client_overrides
            .insert("codex".to_string(), make_override(true, &[("alpha", true)]));
        assert!(cfg.should_run_jack("alpha"));
        // A disabled override wanting it on does NOT count.
        cfg.client_overrides
            .insert("kilo".to_string(), make_override(false, &[("alpha", true)]));
        assert!(cfg.should_run_jack("alpha"), "enabled codex still drives it");
        // Remove codex; only disabled kilo remains + global off -> should not run.
        cfg.client_overrides.remove("codex");
        assert!(!cfg.should_run_jack("alpha"));
        // Flip global on -> should run regardless of overrides.
        cfg.jacks[0].patched = true;
        assert!(cfg.should_run_jack("alpha"));
    }

    // ---- S10c: approval gate defaults + forbidden enforcement + round trip ----

    #[test]
    fn require_approval_for_new_clients_defaults_to_false() {
        // A config that omits the field must load it with the gate OFF (the
        // user-requested default: unknown agents connect straight through, no
        // dialog), and forbidden_clients empty.
        let json = r#"{"version":1,"port":39100}"#;
        let cfg: PatchbayConfig = serde_json::from_str(json).unwrap();
        assert!(
            !cfg.require_approval_for_new_clients,
            "approval gate must default OFF"
        );
        assert!(cfg.forbidden_clients.is_empty());
    }

    #[test]
    fn request_logging_enabled_defaults_to_false_when_absent() {
        // A config that omits the field must load it OFF (the user-required
        // default: no request/event log until the user explicitly opts in via
        // the tray). It must also round-trip when explicitly set true.
        let json = r#"{"version":1,"port":39100}"#;
        let cfg: PatchbayConfig = serde_json::from_str(json).unwrap();
        assert!(
            !cfg.request_logging_enabled,
            "request logging must default OFF when absent"
        );
        // Explicitly enabled round-trips.
        let json_on = r#"{"version":1,"port":39100,"request_logging_enabled":true}"#;
        let cfg_on: PatchbayConfig = serde_json::from_str(json_on).unwrap();
        assert!(cfg_on.request_logging_enabled);
    }

    #[test]
    fn forbidden_client_sees_no_jacks_as_effective_patched() {
        // A forbidden identity's effective_patched resolves to "no access" for
        // EVERY jack, even a globally-patched-on one with no override, and even
        // one a Custom override would otherwise grant.
        let mut cfg = cfg_with_jacks(vec![jack_named("alpha", true), jack_named("beta", false)]);
        cfg.client_overrides
            .insert("codex".to_string(), make_override(true, &[("alpha", true)]));
        cfg.forbidden_clients.push("codex".to_string());

        // codex is forbidden -> effective_patched is false for alpha (override
        // says true but forbidden wins) and beta (global off anyway).
        assert!(!cfg.effective_patched("alpha", Some("codex")));
        assert!(!cfg.effective_patched("beta", Some("codex")));
        // A DIFFERENT (not forbidden) client is unaffected: alpha is on globally.
        assert!(cfg.effective_patched("alpha", Some("claude-code")));
        // No identity is never forbidden.
        assert!(!cfg.is_forbidden(None));
        assert!(cfg.effective_patched("alpha", None));
    }

    #[test]
    fn forbidden_override_does_not_keep_shared_child_running() {
        let mut cfg = cfg_with_jacks(vec![jack_named("alpha", false)]);
        cfg.client_overrides
            .insert("codex".to_string(), make_override(true, &[("alpha", true)]));
        assert!(
            cfg.should_run_jack("alpha"),
            "non-forbidden Custom client drives the shared child"
        );

        cfg.forbidden_clients.push("codex".to_string());
        assert!(
            !cfg.should_run_jack("alpha"),
            "forbidden clients have no access and must not keep a child alive"
        );
        assert_eq!(
            cfg.custom_client_count(),
            0,
            "forbidden overrides should not count as active Custom clients"
        );
    }

    #[test]
    fn forbidden_clients_round_trips_through_json() {
        let mut cfg = cfg_with_jacks(vec![jack_named("alpha", true)]);
        cfg.forbidden_clients.push("bad-agent".to_string());
        cfg.require_approval_for_new_clients = false;
        let s = serde_json::to_string(&cfg).unwrap();
        let back: PatchbayConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(cfg, back);
        assert_eq!(back.forbidden_clients, vec!["bad-agent".to_string()]);
        assert!(!back.require_approval_for_new_clients);
        assert!(back.is_forbidden(Some("bad-agent")));
        assert!(!back.is_forbidden(Some("other")));
    }
}
