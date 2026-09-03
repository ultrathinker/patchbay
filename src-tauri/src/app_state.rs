//! Process-wide shared application state (MASTER_PLAN module tree:
//! `app_state.rs`).
//!
//! Cloned cheaply (all fields are `Arc`) and passed to axum handlers as
//! `State<AppState>`. Holds the live config, the client-session registry, and
//! the gateway lifecycle status.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Instant;

use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use tauri::AppHandle;
use tokio::sync::Notify;

use crate::config::{self, ClientOverride, JackConfig, JackConfigInput, JackTransport, PatchbayConfig};
use crate::gateway::session::SessionRegistry;
use crate::upstream::UpstreamManager;
use crate::utils::log::log;

/// Shared, cloneable application state threaded through Tauri managed state
/// and the axum router.
#[derive(Clone)]
pub struct AppState {
    /// Source of truth: the in-memory config (instant toggle, no restart).
    pub config: Arc<RwLock<PatchbayConfig>>,
    /// Live MCP client sessions (`Mcp-Session-Id` -> session).
    pub sessions: Arc<SessionRegistry>,
    /// Upstream MCP children + merged tool cache (S4: stdio jacks).
    pub upstream: Arc<UpstreamManager>,
    /// Gateway lifecycle status (Starting/Running/Failed/Stopped).
    pub status: Arc<RwLock<GatewayStatus>>,
    /// Wakes the currently-running gateway listener so it can shut down
    /// cleanly before a live port rebind.
    pub shutdown_gateway: Arc<Notify>,
    /// A config parse/IO error surfaced in the tray tooltip. `None` when the
    /// on-disk config parsed cleanly.
    pub config_error: Arc<RwLock<Option<String>>>,
    /// The Tauri app handle, injected after the tray is built (S10) so a
    /// background task (e.g. recording a newly-seen MCP client from the gateway
    /// path, which has no `AppHandle` of its own) can rebuild the tray menu +
    /// refresh the tooltip. `None` until [`AppState::set_tray_handle`] runs.
    pub tray_handle: Arc<RwLock<Option<AppHandle>>>,
    /// (S13 W-D12) The single-slot undo buffer for agent deletion. Holds ONLY
    /// the removed agent entities, never a whole-config snapshot — see
    /// [`AgentTombstone`].
    pub agent_tombstone: Arc<Mutex<Option<AgentTombstone>>>,

    /// (S13) Per-client throttle for `SeenClient::last_seen` persistence: the
    /// last moment we WROTE a fresh timestamp for that identity. A chatty agent
    /// re-initializes often, and every refresh would otherwise rewrite
    /// `patchbay.json`; see [`AppState::touch_client_last_seen`].
    pub last_seen_touch: Arc<Mutex<HashMap<String, Instant>>>,

    /// In-flight first-connection approval dialogs (S10c), keyed by the client
    /// identity the dialog is asking about. A concurrent `initialize` for the
    /// SAME not-yet-decided identity `subscribe()`s to the stored sender instead
    /// of popping a second dialog. Entry is removed the moment the decision is
    /// applied (see [`Self::apply_approval_decision`]).
    pub pending_approvals: Arc<Mutex<HashMap<String, ApprovalSender>>>,
}

/// One pending approval dialog (S10c): a `tokio::sync::watch` sender carrying
/// `None` until the dialog resolves, then `Some(true)` (Allow) or
/// `Some(false)` (Deny). Concurrent `initialize` requests for the same identity
/// `subscribe()` to the same sender so only ONE dialog is ever shown.
type ApprovalSender = tokio::sync::watch::Sender<Option<bool>>;

/// (S13) Minimum interval between two persisted `SeenClient::last_seen`
/// refreshes for the SAME client. A reconnect storm or a chatty agent must not
/// turn `patchbay.json` into a write-hot log; 60 s is far finer than anything
/// the Agents screen renders (relative times and a 30-day "unused" bucket).
pub const LAST_SEEN_THROTTLE_SECS: u64 = 60;

/// Monotonic source of undo tokens. Process-wide and never reused, so a token
/// from an earlier deletion can never accidentally match a later one.
fn next_undo_token() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::SeqCst)
}

/// (S13 W-D12) What one agent-deletion removed, so it can be put back.
///
/// **Entity-scoped on purpose.** Plan v1 proposed keeping a whole
/// `PatchbayConfig` snapshot and restoring it wholesale; the review showed that
/// silently obliterates everything else that changed during the undo window — a
/// jack toggled from the tray, an agent registered by the gateway, a jack added
/// by an MCP meta tool — and desynchronizes the config from running child
/// processes, since writing a config field back does not stop or start
/// anything.
///
/// This carries only the removed identities own records. Restoring them
/// touches nothing else, involves no process lifecycle, and therefore cannot
/// lose a concurrent change.
#[derive(Clone, Debug)]
pub struct AgentTombstone {
    /// Identifies this specific deletion; a stale token is refused so a second
    /// deletion cannot be undone by a click meant for the first.
    pub token: u64,
    /// `seen_clients` rows that were removed (absent for an identity that was
    /// only ever denied).
    pub clients: Vec<crate::config::SeenClient>,
    /// `client_overrides` entries that were removed, with their jack maps.
    pub overrides: Vec<(String, ClientOverride)>,
    /// Identities that were in `forbidden_clients`.
    pub forbidden: Vec<String>,
}

/// Longest client identity Patchbay will keep. Long enough for every real
/// agent name seen in the wild ("Claude Code - Personal", "Antigravity-CLI"),
/// short enough that a name cannot be used to push a UI row off the screen or
/// bloat the config file.
pub const MAX_CLIENT_NAME_LEN: usize = 64;

/// (S13 W-D11, layer 1) Reduce a self-reported client identity to a safe
/// DISPLAY string.
///
/// An agent's identity comes from the `X-Patchbay-Client` header or
/// `clientInfo.name`, and **nothing authenticates either** — any local process
/// that can reach the gateway picks its own. In the native tray menu that was
/// inert: `AppendMenuW` treats a string as text and nothing else. In the S13
/// popover the same string is rendered by a webview, where markup is markup; an
/// identity like `<img src=x onerror=...>` would be script execution inside a
/// process that holds the user's MCP credentials, with the Tauri IPC bridge in
/// reach.
///
/// The frontend also escapes by construction, and the popover's capability
/// grants it almost nothing — this is the first of those layers, not the only
/// one. It is applied at the single point where the identity is resolved, so
/// everything downstream (session, `seen_clients`, `client_overrides`,
/// `forbidden_clients`, the tray label, the log) shares one sanitized value and
/// they cannot disagree about who this is.
///
/// Kept: letters, digits, space, and `. _ - : + @ /` — enough for every real
/// agent name, including versioned and path-like ones.
///
/// Control characters are DROPPED (they have no display value, and an embedded
/// newline would let an agent forge a line in the diagnostic log). Every other
/// disallowed character is REPLACED with `_` rather than deleted: deleting
/// turns `<script>` into the innocuous-looking `script`, whereas `_script_`
/// shows on its face that something was removed.
///
/// This does NOT make identities unforgeable, and is not meant to: two hostile
/// names can still sanitize to the same string, and an agent can always just
/// send a trusted agent's name verbatim. Identity here is self-reported and
/// unauthenticated by design (see `docs/security.md`) — this function's job is
/// to make the string SAFE TO RENDER, not to make it trustworthy.
pub fn sanitize_client_name(raw: &str) -> String {
    let cleaned: String = raw
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| {
            if c.is_alphanumeric()
                || c == ' '
                || c == '.'
                || c == '_'
                || c == '-'
                || c == ':'
                || c == '+'
                || c == '@'
                || c == '/'
            {
                c
            } else {
                '_'
            }
        })
        .take(MAX_CLIENT_NAME_LEN)
        .collect();
    let cleaned = cleaned.trim().to_string();
    if cleaned.is_empty() {
        // A name of nothing but control characters still needs a stable key, or
        // it would be indistinguishable from an unidentified client.
        "unnamed-agent".to_string()
    } else {
        cleaned
    }
}

/// Gateway lifecycle status, surfaced via the tray in later stages.
#[derive(Clone, Debug)]
pub enum GatewayStatus {
    Starting,
    Running { port: u16 },
    Failed { reason: String },
    Stopped,
}

impl AppState {
    /// Build a fresh `AppState` (status `Starting`) from a loaded config.
    pub fn new(config: PatchbayConfig) -> Self {
        AppState {
            config: Arc::new(RwLock::new(config)),
            sessions: Arc::new(SessionRegistry::new()),
            upstream: Arc::new(UpstreamManager::new()),
            status: Arc::new(RwLock::new(GatewayStatus::Starting)),
            shutdown_gateway: Arc::new(Notify::new()),
            config_error: Arc::new(RwLock::new(None)),
            tray_handle: Arc::new(RwLock::new(None)),
            agent_tombstone: Arc::new(Mutex::new(None)),
            last_seen_touch: Arc::new(Mutex::new(HashMap::new())),
            pending_approvals: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Inject the Tauri app handle once the tray exists (S10). Called from
    /// `main.rs` setup shortly after the tray is built. Subsequent calls (e.g.
    /// after a retry-gateway rebind that reuses the same state) are harmless.
    pub fn set_tray_handle(&self, handle: AppHandle) {
        *self.tray_handle.write() = Some(handle);
    }

    /// (S13 W-D4) THE fan-out point: tell every user interface that something
    /// changed.
    ///
    /// Plan v1 proposed pairing a UI event with each `rebuild_menu()` call
    /// site. That was checked against this codebase and found false: there are
    /// only TWO such sites (a config reload and a newly-seen client), and the
    /// hot path — `on_jack_click` → `set_patched` → `set_check` — is not one of
    /// them. Pairing there would have left the window stale on the single most
    /// common action in the app, and the tray stale whenever the window drove
    /// the same action.
    ///
    /// So the fan-out lives HERE, at the end of every mutator, and does both
    /// halves:
    /// - reconciles the tray's check boxes + tooltip against the authoritative
    ///   config (cheap: a handful of `set_checked` calls, no menu rebuild, so
    ///   no Win32 flicker), and
    /// - emits a fresh [`crate::ui::UiSnapshot`] on
    ///   [`crate::ui::STATE_EVENT`] (a no-op when no window exists).
    ///
    /// Put it in the MUTATORS, never in their callers, so a future caller
    /// cannot forget. Structural changes — a jack or client added or removed,
    /// where the menu's SHAPE differs — use
    /// [`Self::notify_structure_changed`] instead.
    ///
    /// Non-blocking: the work is spawned onto the main thread, because a
    /// gateway worker has no `AppHandle` of its own and must never block on the
    /// UI thread while holding a config lock.
    pub fn notify_state_changed(&self) {
        self.fan_out(false);
    }

    /// Like [`Self::notify_state_changed`], but the tray menu's SHAPE changed
    /// (a jack or an agent appeared or disappeared), so the menu is rebuilt
    /// rather than merely reconciled.
    pub fn notify_structure_changed(&self) {
        self.fan_out(true);
    }

    fn fan_out(&self, rebuild: bool) {
        let Some(handle) = self.tray_handle.read().clone() else {
            return; // pre-tray startup, or a unit test: nothing to notify
        };
        tauri::async_runtime::spawn(async move {
            if rebuild {
                crate::tray::rebuild_menu_and_refresh(&handle);
            } else {
                crate::tray::reconcile_checks(&handle);
                crate::tray::refresh_tooltip(&handle);
            }
            if let Some(snapshot) = crate::ui::snapshot_for(&handle) {
                use tauri::Emitter;
                if let Err(e) = handle.emit(crate::ui::STATE_EVENT, snapshot) {
                    log(&format!("notify: emit failed: {}", e));
                }
            }
        });
    }

    /// (S13 W-D12) Delete one or more agent identities, capturing an undo
    /// tombstone first.
    ///
    /// Each identity is purged through the existing [`Self::delete_client`], so
    /// the semantics stay in ONE place; this method only adds the "what was
    /// removed" bookkeeping that makes the window undo strip possible (and that
    /// the tray one-at-a-time delete never needed).
    ///
    /// Returns the undo token. A previous, unused tombstone is discarded — the
    /// strip is single-slot by design, matching what it shows the user.
    pub async fn delete_agents(&self, names: &[String]) -> u64 {
        let tombstone = {
            let cfg = self.config.read();
            let mut clients = Vec::new();
            let mut overrides = Vec::new();
            let mut forbidden = Vec::new();
            for name in names {
                if let Some(c) = cfg.seen_clients.iter().find(|c| c.name == *name) {
                    clients.push(c.clone());
                }
                if let Some(o) = cfg.client_overrides.get(name) {
                    overrides.push((name.clone(), o.clone()));
                }
                if cfg.forbidden_clients.iter().any(|f| f == name) {
                    forbidden.push(name.clone());
                }
            }
            AgentTombstone {
                token: next_undo_token(),
                clients,
                overrides,
                forbidden,
            }
        };
        let token = tombstone.token;
        *self.agent_tombstone.lock() = Some(tombstone);

        for name in names {
            if let Err(e) = self.delete_client(name).await {
                log(&format!("delete_agents: '{}' failed: {}", name, e));
            }
        }
        log(&format!(
            "delete_agents: {} identities purged (undo token {})",
            names.len(),
            token
        ));
        // Each `delete_client` above already fanned out; this final one is the
        // authoritative state after the WHOLE batch, so the UI settles once on
        // the end result instead of on the last agent to be removed.
        self.notify_structure_changed();
        token
    }

    /// (S13 W-D12) Put back exactly what [`Self::delete_agents`] removed.
    ///
    /// Re-inserts the stored entities into the CURRENT config rather than
    /// overwriting it, so any unrelated change made since the deletion survives
    /// untouched. A token that does not match the held tombstone is refused —
    /// that is what makes a stale undo strip harmless.
    pub async fn undo_delete_agents(&self, token: u64) -> Result<usize, String> {
        let tombstone = {
            let mut slot = self.agent_tombstone.lock();
            match slot.as_ref() {
                Some(t) if t.token == token => slot.take().expect("checked above"),
                Some(_) => return Err("this undo is no longer available".to_string()),
                None => return Err("nothing to undo".to_string()),
            }
        };

        let restored = tombstone.clients.len().max(tombstone.forbidden.len());
        let candidate = {
            let cfg = self.config.read();
            let mut snap = cfg.clone();
            let jack_names: Vec<String> = snap.jacks.iter().map(|j| j.name.clone()).collect();

            for c in &tombstone.clients {
                if !snap.seen_clients.iter().any(|x| x.name == c.name) {
                    snap.seen_clients.push(c.clone());
                    // An identity that is known cannot also be denied: if it was
                    // denied at the gate while it sat deleted, restoring the
                    // "seen" record without clearing that would put it in both
                    // lists at once.
                    snap.forbidden_clients.retain(|f| *f != c.name);
                }
            }

            for (name, ovr) in &tombstone.overrides {
                // Re-key the restored map against the jacks that exist NOW. A
                // jack removed while the agent was deleted must not come back as
                // a phantom entry, and a jack added meanwhile needs a value, or
                // the override map stops mirroring the global list — an
                // invariant the rest of the config code relies on.
                let mut rekeyed = ovr.clone();
                rekeyed.jacks.retain(|jack, _| jack_names.contains(jack));
                for jack in &jack_names {
                    rekeyed
                        .jacks
                        .entry(jack.clone())
                        .or_insert_with(|| snap.effective_patched(jack, None));
                }
                snap.client_overrides
                    .entry(name.clone())
                    .or_insert(rekeyed);
            }

            for f in &tombstone.forbidden {
                // Only restore a denial for an identity that has not since been
                // deliberately re-admitted; silently re-blocking an agent the
                // user just allowed would be worse than not undoing at all.
                let re_admitted = snap.seen_clients.iter().any(|c| c.name == *f)
                    && !tombstone.clients.iter().any(|c| c.name == *f);
                if !re_admitted && !snap.forbidden_clients.iter().any(|x| x == f) {
                    snap.forbidden_clients.push(f.clone());
                }
            }
            snap
        };
        self.persist(&candidate)?;
        *self.config.write() = candidate;

        // A restored Custom override can be the only reason a globally-off jack
        // needs to run. `delete_client` reconciles on the way out; undoing it
        // has to reconcile on the way back in, or the child stays dead and every
        // agent keeps a stale tool list.
        self.reconcile_all_jack_lifecycles().await;
        self.sessions.broadcast_tools_list_changed().await;

        log(&format!(
            "undo_delete_agents: {} identities restored",
            restored
        ));
        self.notify_structure_changed();
        Ok(restored)
    }

    /// (S13 §3.3) Discard a client preserved Custom list and start again from
    /// the global one.
    ///
    /// The tray cannot express this: `disable_custom_client` keeps the jack map
    /// so re-enabling restores it, and there is no path back to "just follow the
    /// global list". Removing the override entry entirely IS that path — the
    /// next `enable_custom_client` then seeds fresh from global, because seeding
    /// only happens when no entry exists.
    pub async fn reset_custom_to_global(&self, client_name: &str) -> Result<(), String> {
        let candidate = {
            let cfg = self.config.read();
            if !cfg.client_overrides.contains_key(client_name) {
                return Ok(()); // already following the global list
            }
            let mut snap = cfg.clone();
            snap.client_overrides.remove(client_name);
            snap
        };
        self.persist(&candidate)?;
        // Commit only the collection this method owns. Assigning the whole
        // candidate would erase anything else that changed while the file was
        // being written.
        {
            let overrides = candidate.client_overrides.clone();
            self.config.write().client_overrides = overrides;
        }
        self.reconcile_all_jack_lifecycles().await;
        log(&format!(
            "reset_custom_to_global: '{}' now follows the global list",
            client_name
        ));
        self.notify_structure_changed();
        Ok(())
    }

    /// (S13) Persist `autostart` and update the Windows Run key.
    ///
    /// Lifted out of the tray click handler so the window drives the SAME code
    /// rather than a second copy — a duplicated implementation is exactly how
    /// two UIs start disagreeing. Save-then-commit; the registry write happens
    /// only after the config is safely on disk, so a failed save cannot leave
    /// Patchbay launching at boot with a config that says it should not.
    pub fn set_autostart(&self, enabled: bool) -> Result<(), String> {
        let candidate = {
            let cfg = self.config.read();
            if cfg.autostart == enabled {
                return Ok(());
            }
            let mut snap = cfg.clone();
            snap.autostart = enabled;
            snap
        };
        self.persist(&candidate)?;
        self.config.write().autostart = enabled;

        if let Err(e) = crate::utils::autorun::set_autorun(enabled) {
            log(&format!("set_autostart: set_autorun({}) failed: {}", enabled, e));
        }
        self.notify_state_changed();
        Ok(())
    }

    /// (S13) Persist `require_approval_for_new_clients`. Save-then-commit; no
    /// other side effects — OFF means an unknown identity is auto-recorded (the
    /// pre-S10c behavior), ON means it waits for the approval dialog.
    pub fn set_require_approval(&self, enabled: bool) -> Result<(), String> {
        let candidate = {
            let cfg = self.config.read();
            if cfg.require_approval_for_new_clients == enabled {
                return Ok(());
            }
            let mut snap = cfg.clone();
            snap.require_approval_for_new_clients = enabled;
            snap
        };
        self.persist(&candidate)?;
        self.config.write().require_approval_for_new_clients = enabled;
        self.notify_state_changed();
        Ok(())
    }

    /// (S13) Change the gateway port: persist, then rebind the listener.
    ///
    /// Rebinding drops every live MCP session, which is why this is the one
    /// setting in the window behind an explicit Apply rather than applying on
    /// click. Save-then-commit: if the config cannot be written, nothing is
    /// rebound, because a listener on a port the config does not record is a
    /// state no restart could reproduce.
    ///
    /// The listener swap mirrors the reload path: close long-lived SSE
    /// responses FIRST (otherwise graceful shutdown waits forever on a client
    /// still attached to the old listener), notify the running gateway to stop,
    /// then spawn a fresh one.
    pub async fn set_port(&self, port: u16) -> Result<(), String> {
        if port == 0 {
            return Err("port must be between 1 and 65535".to_string());
        }
        let candidate = {
            let cfg = self.config.read();
            if cfg.port == port {
                return Ok(());
            }
            let mut snap = cfg.clone();
            snap.port = port;
            snap
        };
        self.persist(&candidate)?;
        let old = self.config.read().port;
        self.config.write().port = port;
        log(&format!("set_port: {} -> {}, rebinding listener", old, port));

        self.sessions.close_all_streams();
        self.shutdown_gateway.notify_waiters();

        let gw_state = self.clone();
        tauri::async_runtime::spawn(async move {
            crate::gateway::run_gateway(gw_state, port).await;
        });

        // Let the fresh bind settle so the snapshot that follows reports
        // Running (or Failed) on the NEW port rather than the stale status.
        tokio::time::sleep(std::time::Duration::from_millis(750)).await;
        crate::utils::request_log::log_event(self, &format!("gateway_port_rebind {}", port));
        self.notify_state_changed();
        Ok(())
    }

    /// (S13 fix) Deny or allow SEVERAL identities in ONE save-and-commit cycle.
    ///
    /// The window's multi-select used to call [`Self::set_forbidden`] once per
    /// agent. Each of those calls clones the config, writes the whole file, and
    /// then commits — so twenty concurrent calls each carried a candidate
    /// holding only its OWN change, raced to disk, and whichever finished last
    /// erased the other nineteen denials. It also meant twenty file writes and
    /// twenty Win32 menu rebuilds for one click.
    ///
    /// One read, one save, one commit, one reconcile, one notify.
    pub async fn set_forbidden_batch(
        &self,
        identities: &[String],
        forbidden: bool,
    ) -> Result<usize, String> {
        if identities.is_empty() {
            return Ok(0);
        }
        let (candidate, changed) = {
            let cfg = self.config.read();
            let mut snap = cfg.clone();
            let before = snap.forbidden_clients.len();
            if forbidden {
                for id in identities {
                    if !snap.forbidden_clients.iter().any(|f| f == id) {
                        snap.forbidden_clients.push(id.clone());
                    }
                }
            } else {
                snap.forbidden_clients.retain(|f| !identities.contains(f));
            }
            let changed = snap.forbidden_clients.len() != before;
            (snap, changed)
        };
        if !changed {
            return Ok(0);
        }
        self.persist(&candidate)?;
        let list = candidate.forbidden_clients.clone();
        // Commit ONLY the collection this method owns, so a concurrent change to
        // an unrelated field made during the disk write is not clobbered.
        self.config.write().forbidden_clients = list;

        self.reconcile_all_jack_lifecycles().await;
        self.sessions.broadcast_tools_list_changed().await;
        crate::utils::request_log::log_event(
            self,
            &format!(
                "set_forbidden_batch {} identities -> forbidden={}",
                identities.len(),
                forbidden
            ),
        );
        self.notify_structure_changed();
        Ok(identities.len())
    }

    /// (S13) Persist a new [`crate::config::UiMode`]. Save-then-commit; the
    /// caller re-points the tray icon's buttons afterwards
    /// ([`crate::tray::apply_ui_mode`]).
    pub fn set_ui_mode(&self, mode: crate::config::UiMode) -> Result<(), String> {
        let candidate = {
            let cfg = self.config.read();
            if cfg.ui_mode == mode {
                return Ok(()); // no-op: no needless save
            }
            let mut snap = cfg.clone();
            snap.ui_mode = mode;
            snap
        };
        self.persist(&candidate)?;
        self.config.write().ui_mode = mode;
        log(&format!("ui_mode set to '{}'", mode.as_str()));
        // In the MUTATOR, not the caller — the rule this file states two
        // methods above, and which this method was quietly breaking.
        self.notify_state_changed();
        Ok(())
    }

    /// Snapshot of every jack for the tray menu + tooltip: name + patched flag.
    pub fn jack_lines(&self) -> Vec<JackLine> {
        let cfg = self.config.read();
        cfg.jacks
            .iter()
            .map(|j| JackLine {
                name: j.name.clone(),
                patched: j.patched,
            })
            .collect()
    }

    /// THE one place a jack's GLOBAL flag flips (MASTER_PLAN D3 sequencing).
    /// Both the tray `CheckMenuItem` handler and the `POST /debug/toggle` test
    /// hook route through here so there is a single source of truth.
    ///
    /// 1. Flip the in-memory config (`patched`) and persist to disk
    ///    (`config::save`).
    /// 2. Reconcile the shared child's lifecycle against `should_run_jack` (S10):
    ///    the child runs iff the GLOBAL flag OR any enabled Custom client needs
    ///    it. So toggling the global flag OFF does NOT kill a child some Custom
    ///    client still relies on, and toggling it ON is a no-op if the child is
    ///    already alive (kept by a Custom client). A lifecycle change still
    ///    broadcasts `tools/list_changed` (start-then-broadcast for ON,
    ///    broadcast-then-stop for OFF); a no-op lifecycle change (only the
    ///    per-client visibility shifted) still broadcasts so `tools/list`
    ///    re-evaluates `effective_patched`.
    /// 3. Return the resulting GLOBAL `patched` flag + status string so callers
    ///    can reconcile the UI (the tray `set_checked`, the tooltip).
    pub async fn set_patched(&self, jack_name: &str, patched: bool) -> ToggleResult {
        // (a) Serialize per jack: hold this jack's toggle lock for the whole
        // flip + start/stop so a fast double-toggle or a toggle racing reload
        // can't spawn two children (one leaked).
        let _toggle_guard = self.upstream.jack_lock(jack_name).await;
        // 1. Flip config + persist (no guard held across the await below).
        //
        // (S13 §6.1) SAVE-THEN-COMMIT, matching `set_forbidden` /
        // `delete_client` / `disable_custom_client`. This method used to mutate
        // the LIVE config first and merely log a failed `config::save`, so a
        // failed persist left runtime and disk disagreeing — on the single most
        // frequent operation in the app, and with the window UI about to render
        // that wrong in-memory value confidently.
        //
        // The commit deliberately writes back ONLY this jack's flag rather than
        // assigning the whole candidate snapshot: an unrelated field changed by
        // another task while we were writing to disk must not be clobbered in
        // memory. (The on-disk copy can still lag such a change until the next
        // save — that is inherent to the existing snapshot-save discipline used
        // everywhere in this file, not something introduced here.)
        let jack_config = {
            let candidate = {
                let cfg = self.config.read();
                if !cfg.jacks.iter().any(|j| j.name == jack_name) {
                    log(&format!("set_patched: unknown jack '{}'", jack_name));
                    return ToggleResult {
                        patched: false,
                        status: "unknown".to_string(),
                    };
                }
                let mut snap = cfg.clone();
                if let Some(j) = snap.jacks.iter_mut().find(|j| j.name == jack_name) {
                    j.patched = patched;
                }
                snap
            };

            if let Err(e) = self.persist(&candidate) {
                log(&format!(
                    "set_patched: failed to persist config, NOT committing: {}",
                    e
                ));
                // Report the UNCHANGED authoritative flag so the tray check box
                // and the window switch both reconcile back to reality.
                let current = self
                    .config
                    .read()
                    .jacks
                    .iter()
                    .find(|j| j.name == jack_name)
                    .map(|j| j.patched)
                    .unwrap_or(false);
                return ToggleResult {
                    patched: current,
                    status: format!("failed to persist: {}", e),
                };
            }

            {
                let mut cfg = self.config.write();
                if let Some(j) = cfg.jacks.iter_mut().find(|j| j.name == jack_name) {
                    j.patched = patched;
                }
            }

            // Re-read the (now-flipped) jack config for a potential start.
            self.config
                .read()
                .jacks
                .iter()
                .find(|j| j.name == jack_name)
                .cloned()
        };

        let jack_config = match jack_config {
            Some(j) => j,
            None => {
                return ToggleResult {
                    patched: false,
                    status: "unknown".to_string(),
                }
            }
        };

        // 2. Decide the child's lifecycle via should_run (S10): GLOBAL OR any
        //    enabled Custom client needing it. Reconcile against the CURRENT
        //    runtime state so an already-alive child is never pointlessly
        //    restarted and a still-needed child is never killed.
        let was_running = self.upstream.is_jack_running(jack_name);
        let should_run = self.config.read().should_run_jack(jack_name);

        if should_run && !was_running {
            // Turn ON: start (spawn + handshake + cache) THEN broadcast.
            self.upstream
                .start_jack(&jack_config, self.sessions.clone(), self.config.clone())
                .await;
            self.sessions.broadcast_tools_list_changed().await;
        } else if !should_run && was_running {
            // Turn OFF: broadcast IMMEDIATELY (enforcement is already active),
            // THEN kill the child.
            self.sessions.broadcast_tools_list_changed().await;
            self.upstream.stop_jack(jack_name).await;
        } else {
            // No lifecycle change, but per-client visibility may have shifted
            // (e.g. global OFF while a Custom client keeps the child alive ->
            // global-default clients must now hide it). Broadcast so tools/list
            // re-evaluates effective_patched.
            self.sessions.broadcast_tools_list_changed().await;
        }

        let status = self.upstream.status_string(jack_name);
        if status.starts_with("failed:") {
            crate::utils::request_log::log_error(
                self,
                &format!("jack_start_failed '{}' ({})", jack_name, status),
            );
        }
        crate::utils::request_log::log_event(
            self,
            &format!("toggle_jack '{}' -> patched={}", jack_name, patched),
        );
        // (S13 W-D4) Fan out to EVERY interface from the mutator itself, so
        // a future caller cannot forget.
        self.notify_state_changed();
        ToggleResult {
            patched,
            status,
        }
    }

    // ---- S8: add / remove / list jacks (admin API + meta MCP tools) --------
    //
    // These sit on top of the SAME primitives `set_patched` uses (config lock +
    // `config::save` + `upstream::start_jack`/`stop_jack` + per-jack lock + the
    // session broadcast) so a jack added/removed at runtime behaves identically
    // to one toggled in the tray: persist + start/stop + broadcast, no restart.

    /// Add a new jack from an admin/meta-tool input (S8).
    ///
    /// 1. Validate the name (`is_valid_jack_name`).
    /// 2. Validate the transport's required field (stdio: `command`;
    ///    streamable-http: `url`).
    /// 3. Acquire the per-jack lock, then reject duplicate names UNDER it (so
    ///    two concurrent adds of the same name can't both insert).
    /// 4. Insert into the live config + persist via `config::save` (which
    ///    DPAPI-encrypts plaintext secrets, same as everywhere else).
    /// 5. Start the upstream (same path as toggle ON) when `patched`.
    /// 6. Broadcast `notifications/tools/list_changed` so already-connected
    ///    clients see the new tools on their next `tools/list`.
    ///
    /// A failed start does NOT roll back the config write — the jack stays
    /// (persisted) with a Failed/Stopped status surfaced in the returned
    /// summary. Only validation/persist failures return `Err`.
    pub async fn add_jack(
        &self,
        input: JackConfigInput,
    ) -> Result<JackSummary, AddJackError> {
        // 1. Name validity (cheap, no lock needed).
        if !config::is_valid_jack_name(&input.name) {
            return Err(AddJackError::InvalidName(input.name.clone()));
        }
        // 2. Transport required-field validation (no lock needed).
        validate_transport(&input.transport)?;

        // Serialize the mutation + start per jack (same lock discipline as
        // `set_patched`): a concurrent toggle/reload on this name can't race our
        // start_jack, and two concurrent adds of the same name can't both pass
        // the duplicate check below.
        let _guard = self.upstream.jack_lock(&input.name).await;

        // 3. Duplicate check UNDER the lock (a concurrent add/reload may have
        //    just inserted this name since the caller built the input).
        {
            let cfg = self.config.read();
            if cfg.jacks.iter().any(|j| j.name == input.name) {
                return Err(AddJackError::DuplicateName(input.name.clone()));
            }
        }

        // 4. Build the full JackConfig, insert under the config lock, persist.
        //    Clone the jack for the potential start below (the one in the config
        //    is owned by the Vec).
        let jack = JackConfig {
            name: input.name.clone(),
            patched: input.patched,
            transport: input.transport.clone(),
            sharing: input.sharing.clone(),
            tools: None,
        };
        let jack_for_start = jack.clone();
        let jack_name = jack.name.clone();
        let new_patched = input.patched;
        // Build the candidate snapshot from a READ of the live config (no
        // mutation yet) and persist it FIRST. Only commit to the in-memory
        // config once the disk write actually succeeds, so a failed
        // `persist` can never leave memory and disk diverged.
        {
            let snapshot = {
                let cfg = self.config.read();
                let mut snap = cfg.clone();
                snap.jacks.push(jack);
                // (S10) Keep every Custom client's list in sync with the global
                // jack set: seed the new jack (with the SAME patched value it
                // was created with) into every override's jacks map, whether or
                // not that override is currently enabled (cheap, avoids surprises
                // if a disabled one is re-enabled later).
                for ovr in snap.client_overrides.values_mut() {
                    ovr.jacks.insert(jack_name.clone(), new_patched);
                }
                snap
            };
            if let Err(e) = self.persist(&snapshot) {
                log(&format!("add_jack: failed to persist config: {}", e));
                return Err(AddJackError::PersistFailed(e));
            }
            *self.config.write() = snapshot;
        }

        // 5. Start the upstream when patched (same path as toggle ON). For a
        //    freshly-added jack `patched == should_run_jack` (it was just seeded
        //    identically into every override), so the raw flag is authoritative
        //    here. A failed start records Failed/Stopped inside `start_jack` and
        //    is surfaced via the summary; the config write is NOT rolled back.
        if jack_for_start.patched {
            self.upstream
                .start_jack(
                    &jack_for_start,
                    self.sessions.clone(),
                    self.config.clone(),
                )
                .await;
        }

        // 6. Broadcast so connected clients refresh.
        self.sessions.broadcast_tools_list_changed().await;

        // 7. Summary reflects the actual start outcome.
        let summary = self.jack_summary(&jack_name);
        if summary.status.starts_with("failed:") {
            crate::utils::request_log::log_error(
                self,
                &format!("jack_start_failed '{}' ({})", summary.name, summary.status),
            );
        }
        crate::utils::request_log::log_event(
            self,
            &format!("add_jack '{}' ({})", summary.name, summary.transport),
        );
        // (S13 W-D4) Fan out to EVERY interface from the mutator itself, so
        // a future caller cannot forget.
        // A new jack changes the MENU SHAPE, so the tray is rebuilt.
        self.notify_structure_changed();
        Ok(summary)
    }

    /// Remove a jack by name (S8). Stops any running upstream client (same
    /// shutdown path as toggle OFF, under the per-jack lock so it can't race a
    /// concurrent toggle), removes it from the config, persists, and broadcasts
    /// `tools/list_changed`.
    pub async fn remove_jack(&self, name: &str) -> Result<(), RemoveJackError> {
        // 1. Existence check (NotFound before touching anything).
        let exists = {
            let cfg = self.config.read();
            cfg.jacks.iter().any(|j| j.name == name)
        };
        if !exists {
            return Err(RemoveJackError::NotFound(name.to_string()));
        }

        // 2. Serialize per jack (same lock discipline as set_patched).
        let _guard = self.upstream.jack_lock(name).await;

        // 3. Stop the running upstream client (same shutdown path).
        self.upstream.stop_jack(name).await;

        // 4. Remove from config + persist. Build the candidate snapshot from a
        //    READ (no mutation yet), persist it FIRST, and only commit to the
        //    in-memory config once the disk write succeeds — same
        //    save-before-commit discipline as `add_jack`.
        {
            let snapshot = {
                let cfg = self.config.read();
                let mut snap = cfg.clone();
                snap.jacks.retain(|j| j.name != name);
                // (S10) Remove the jack from every Custom client's list too, so
                // every Custom list always mirrors the global jack NAME set.
                for ovr in snap.client_overrides.values_mut() {
                    ovr.jacks.remove(name);
                }
                snap
            };
            if let Err(e) = self.persist(&snapshot) {
                log(&format!("remove_jack: failed to persist config: {}", e));
                return Err(RemoveJackError::PersistFailed(e));
            }
            *self.config.write() = snapshot;
        }

        // 5. Broadcast so connected clients drop the jack's tools.
        self.sessions.broadcast_tools_list_changed().await;
        crate::utils::request_log::log_event(self, &format!("remove_jack '{}'", name));
        // (S13 W-D4) Fan out to EVERY interface from the mutator itself, so
        // a future caller cannot forget.
        self.notify_structure_changed();
        Ok(())
    }

    /// Snapshot of every jack (S8): name, patched, transport type, runtime
    /// status, and tool count. Used by the meta tools + admin HTTP endpoints so
    /// an agent/script can read Patchbay's state without opening the config.
    pub fn list_jacks(&self) -> Vec<JackSummary> {
        // Snapshot per-jack tool counts first (one runtimes read lock, then
        // dropped) so the config read below never nests a runtimes lock via
        // status_string in an inconsistent order.
        let mut tool_counts: HashMap<String, usize> = HashMap::new();
        for (name, _tool) in self.upstream.cached_tools() {
            *tool_counts.entry(name).or_insert(0) += 1;
        }

        let cfg = self.config.read();
        cfg.jacks
            .iter()
            .map(|j| JackSummary {
                name: j.name.clone(),
                patched: j.patched,
                transport: transport_type_string(&j.transport),
                status: self.upstream.status_string(&j.name),
                tool_count: tool_counts.get(&j.name).copied().unwrap_or(0),
            })
            .collect()
    }

    /// Build one jack's summary (add_jack's return value). Snapshots tool count
    /// + status under brief locks (runtimes), then the config fields (config),
    /// with no lock held across the other.
    fn jack_summary(&self, name: &str) -> JackSummary {
        let tool_count = self
            .upstream
            .cached_tools()
            .iter()
            .filter(|(n, _)| n == name)
            .count();
        let status = self.upstream.status_string(name);
        let (patched, transport) = {
            let cfg = self.config.read();
            match cfg.jacks.iter().find(|j| j.name == name) {
                Some(j) => (j.patched, transport_type_string(&j.transport)),
                None => (false, String::new()),
            }
        };
        JackSummary {
            name: name.to_string(),
            patched,
            transport,
            status,
            tool_count,
        }
    }

    // ---- S10: per-client ("Custom") MCP-server permission lists ------------
    //
    // `set_client_override` flips ONE jack for ONE client's own list (lazily
    // creating the override — enabled, seeded with a snapshot of the current
    // global list — the first time a client is customized). `enable_custom_client`
    // turns Custom mode on with no jack change yet. Both persist via
    // `config::save` (same discipline as `set_patched`/`add_jack`) and broadcast
    // `notifications/tools/list_changed`. Override authorship stays tray-only
    // (human-gated): there is NO `patchbay__set_client_override` meta tool.

    /// Record a connecting client's `clientInfo.name` on first sight (S10).
    /// Append-only: a name already in `seen_clients` is never duplicated or
    /// rewritten, so reconnects of a known agent never trigger a needless config
    /// save. On a genuinely NEW name, persist + trigger a tray menu rebuild (via
    /// the injected [`Self::tray_handle`]) so the agent appears in the "Custom"
    /// submenu without a manual "Reload config". `version` is display-only.
    pub fn record_seen_client(&self, name: &str, version: Option<&str>) {
        if name.is_empty() {
            return;
        }
        // Fast path: already known (a cheap read, no write).
        let already_known = {
            let cfg = self.config.read();
            cfg.seen_clients.iter().any(|c| c.name == name)
        };
        if already_known {
            return;
        }
        // Slow path: append + persist under the write lock, re-checking to win
        // the race against a concurrent first-sighting of the same name. Both
        // early-return paths above/below exit before this point, so reaching
        // past this block always means a new client was actually recorded.
        {
            let now = chrono::Local::now().to_rfc3339();
            let mut cfg = self.config.write();
            if cfg.seen_clients.iter().any(|c| c.name == name) {
                return;
            }
            cfg.seen_clients.push(crate::config::SeenClient {
                name: name.to_string(),
                first_seen_version: version.map(str::to_owned),
                first_seen: now.clone(),
                // A first sighting IS a sighting: seed last_seen so a client
                // that connects once and never returns is immediately
                // distinguishable from one that is still in use.
                last_seen: Some(now),
            });
            let snap = cfg.clone();
            drop(cfg);
            // (S14) State only: a new agent is an observation, not a
            // configuration change.
            if let Err(e) = self.persist_state(&snap) {
                log(&format!("record_seen_client: failed to persist: {}", e));
            }
        }
        {
            log(&format!("record_seen_client: new client '{}' recorded", name));
            // (S13) A new agent changes the menu SHAPE (it appears in the
            // "Custom" submenu) AND the window's Agents list, so the one
            // fan-out point handles both. Replaces the hand-rolled spawn +
            // rebuild that predated the state bus.
            self.notify_structure_changed();
        }
    }

    /// (S13) Refresh `SeenClient::last_seen` for an already-known client.
    ///
    /// Called on the `initialize` path for EVERY identified client, including
    /// ones already in `seen_clients` — [`Self::record_seen_client`]
    /// deliberately returns early for a known name (it is append-only), so
    /// before S13 nothing in Patchbay ever recorded that an identity was STILL
    /// in use. That is exactly what made 60 junk one-shot script identities
    /// indistinguishable from the handful of live agents in the tray list.
    ///
    /// Throttled to one persist per client per [`LAST_SEEN_THROTTLE_SECS`]; a
    /// throttled call is a pure no-op (no config lock, no disk write). An
    /// unknown name is ignored — recording a NEW client stays
    /// [`Self::record_seen_client`]'s job (and goes through the approval gate
    /// first), so this can never resurrect a deleted or denied identity.
    ///
    /// Save-then-commit like every other mutator: the candidate is persisted
    /// before the live config is touched, and only this one field is written
    /// back so a concurrent change elsewhere is not clobbered.
    pub fn touch_client_last_seen(&self, name: &str) {
        {
            let mut touched = self.last_seen_touch.lock();
            if let Some(prev) = touched.get(name) {
                if prev.elapsed().as_secs() < LAST_SEEN_THROTTLE_SECS {
                    return;
                }
            }
            touched.insert(name.to_string(), Instant::now());
        }

        let now = chrono::Local::now().to_rfc3339();
        let candidate = {
            let cfg = self.config.read();
            if !cfg.seen_clients.iter().any(|c| c.name == name) {
                return; // never-seen identity: not this method's business
            }
            let mut snap = cfg.clone();
            if let Some(c) = snap.seen_clients.iter_mut().find(|c| c.name == name) {
                c.last_seen = Some(now.clone());
            }
            snap
        };

        // (S14) State only. This is the write that fires roughly once a minute
        // per active agent; it is precisely what must never reach the config.
        if let Err(e) = self.persist_state(&candidate) {
            log(&format!(
                "touch_client_last_seen: failed to persist for '{}': {}",
                name, e
            ));
            return;
        }

        {
            let mut cfg = self.config.write();
            if let Some(c) = cfg.seen_clients.iter_mut().find(|c| c.name == name) {
                c.last_seen = Some(now);
            }
        }
        // (S13 fix) This is a user-visible change — the Agents screen renders
        // "active 2 minutes ago" from it — so it fans out like every other
        // mutator. Throttled to once a minute per client, so this cannot become
        // a notification storm.
        self.notify_state_changed();
    }

    /// Persist a candidate config — the ONLY way this type writes to disk.
    ///
    /// (B-1) When the file on disk failed to parse at startup, the config held
    /// in memory is [`crate::config::safe_default`]: no jacks, no known agents,
    /// no secrets. `main` deliberately does not save it — but every mutator
    /// did, and the mutators are not all user-initiated. One agent connecting
    /// is enough to reach `record_seen_client`, and `touch_client_last_seen`
    /// fires roughly once a minute per active agent. So a corrupt config was a
    /// timer: within about a minute of starting, the empty default would be
    /// written over the real file — every server definition and every encrypted
    /// secret gone, with no backup.
    ///
    /// Refusing the write keeps the app fully usable (the tray already shows
    /// the parse error, and "Reload config" clears it once the file is fixed)
    /// while the file on disk stays exactly as the user left it.
    fn persist(&self, candidate: &config::PatchbayConfig) -> Result<(), String> {
        if let Some(reason) = self.config_error.read().as_ref() {
            let msg = format!(
                "refusing to save: the config on disk did not parse ({}), so \
                 memory holds an empty default. Fix the file and use Reload \
                 config.",
                reason
            );
            log(&format!("persist: {}", msg));
            return Err(msg);
        }
        config::save(candidate)
    }

    /// Persist ONLY the observed-agents file (S14), leaving `patchbay.json`
    /// alone.
    ///
    /// For the two mutators driven by traffic rather than by the user:
    /// [`Self::record_seen_client`] and [`Self::touch_client_last_seen`]. They
    /// change nothing but `seen_clients`, which no longer lives in the config
    /// file, so writing the config file for them would rewrite every server
    /// definition and every encrypted secret several times an hour to record
    /// something nobody configured.
    ///
    /// Guarded like [`Self::persist`], for the same reason: when the config on
    /// disk did not parse, memory holds an empty default, and its empty
    /// `seen_clients` must not be written over a real one.
    fn persist_state(&self, candidate: &config::PatchbayConfig) -> Result<(), String> {
        if let Some(reason) = self.config_error.read().as_ref() {
            return Err(format!(
                "refusing to save: the config on disk did not parse ({})",
                reason
            ));
        }
        config::save_runtime_state(candidate)
    }

    /// Flip ONE jack for ONE client's override list (S10). Lazily creates the
    /// `ClientOverride` entry — `enabled: true` with a FULL jacks-map snapshot
    /// of the current global list — if this is the first time the client is
    /// customized (so "turn Custom on for a client" and "edit its first jack"
    /// are the same action), then sets the one value. All OTHER jacks in the new
    /// list default to a copy of their current global value at that moment.
    /// Persists + broadcasts. Returns the resulting effective value for the
    /// (client, jack) so the tray can reconcile the checkbox.
    pub async fn set_client_override(
        &self,
        client_name: &str,
        jack_name: &str,
        patched: bool,
    ) -> Result<bool, String> {
        let _toggle_guard = self.upstream.jack_lock(jack_name).await;

        let jack_config = {
            let mut cfg = self.config.write();
            if !cfg.jacks.iter().any(|j| j.name == jack_name) {
                return Err(format!("jack '{}' not found", jack_name));
            }
            // Compute the full seed snapshot up front (needed only if this client
            // has no override yet), OUTSIDE the entry borrow to satisfy the
            // borrow checker (can't read cfg.jacks while borrowing
            // cfg.client_overrides).
            let seed: BTreeMap<String, bool> = cfg
                .jacks
                .iter()
                .map(|j| (j.name.clone(), j.patched))
                .collect();
            let entry = cfg
                .client_overrides
                .entry(client_name.to_string())
                .or_insert_with(|| ClientOverride {
                    enabled: true,
                    jacks: seed,
                });
            entry.enabled = true;
            entry.jacks.insert(jack_name.to_string(), patched);
            let snap = cfg.clone();
            drop(cfg);
            if let Err(e) = self.persist(&snap) {
                log(&format!("set_client_override: failed to persist: {}", e));
                return Err(e);
            }
            self.config
                .read()
                .jacks
                .iter()
                .find(|j| j.name == jack_name)
                .cloned()
        };

        let Some(jack_config) = jack_config else {
            return Err(format!("jack '{}' not found", jack_name));
        };

        // Reconcile the SHARED child lifecycle against the aggregate
        // should_run_jack value. A globally-off jack enabled for this Custom
        // client must start; disabling the last Custom consumer of a globally-off
        // jack must stop it. Broadcast ordering matches set_patched.
        let was_running = self.upstream.is_jack_running(jack_name);
        let should_run = self.config.read().should_run_jack(jack_name);
        if should_run && !was_running {
            self.upstream
                .start_jack(&jack_config, self.sessions.clone(), self.config.clone())
                .await;
            self.sessions.broadcast_tools_list_changed().await;
        } else if !should_run && was_running {
            self.sessions.broadcast_tools_list_changed().await;
            self.upstream.stop_jack(jack_name).await;
        } else {
            // No lifecycle change, but this client's effective tools/list
            // changed, so every session should re-evaluate.
            self.sessions.broadcast_tools_list_changed().await;
        }
        // Re-read the authoritative effective value (defensive: the jack may not
        // exist in the global list, in which case effective_patched falls back).
        let effective = self
            .config
            .read()
            .effective_patched(jack_name, Some(client_name));
        // (S13 W-D4) Fan out to EVERY interface from the mutator itself.
        // Flipping the FIRST jack for a client lazily creates its override,
        // which changes the "Custom [n/m]" label — cheaper to always rebuild
        // here than to reason about whether this call was the first one.
        self.notify_structure_changed();
        Ok(effective)
    }

    /// Turn "Custom" mode ON for a client with NO jack change yet (S10): seeds
    /// a full snapshot of the current global list (so every jack starts at its
    /// global value) and sets `enabled: true`. If the client already has an
    /// override entry, just flip `enabled` on (its list is kept as-is). Persists
    /// + broadcasts. Used when the tray wants an explicit "start customizing
    /// this agent" action separate from editing a jack.
    pub async fn enable_custom_client(&self, client_name: &str) -> Result<(), String> {
        let affected_jacks = {
            let cfg = self.config.read();
            cfg.jacks.iter().map(|j| j.name.clone()).collect::<Vec<_>>()
        };
        {
            let mut cfg = self.config.write();
            // Check membership with a short-lived immutable borrow (ended before
            // the mutation below), avoiding a get_mut borrow held across the
            // else branch (an NLL limitation).
            let exists = cfg.client_overrides.contains_key(client_name);
            if exists {
                let ovr = cfg.client_overrides.get_mut(client_name).expect("checked above");
                if ovr.enabled {
                    return Ok(()); // already Custom; no-op (no needless save).
                }
                ovr.enabled = true;
            } else {
                let jacks: BTreeMap<String, bool> = cfg
                    .jacks
                    .iter()
                    .map(|j| (j.name.clone(), j.patched))
                    .collect();
                cfg.client_overrides.insert(
                    client_name.to_string(),
                    ClientOverride {
                        enabled: true,
                        jacks,
                    },
                );
            }
            let snap = cfg.clone();
            drop(cfg);
            if let Err(e) = self.persist(&snap) {
                log(&format!("enable_custom_client: failed to persist: {}", e));
                return Err(e);
            }
        }

        // Enabling a previously-disabled override can make one or more
        // globally-off jacks needed again if its preserved Custom list had them
        // on. Reconcile each configured jack.
        for jack_name in affected_jacks {
            let _toggle_guard = self.upstream.jack_lock(&jack_name).await;
            let jack_config = {
                let cfg = self.config.read();
                cfg.jacks.iter().find(|j| j.name == jack_name).cloned()
            };
            let Some(jack_config) = jack_config else {
                continue;
            };
            let was_running = self.upstream.is_jack_running(&jack_name);
            let should_run = self.config.read().should_run_jack(&jack_name);
            if should_run && !was_running {
                self.upstream
                    .start_jack(&jack_config, self.sessions.clone(), self.config.clone())
                    .await;
            } else if !should_run && was_running {
                self.upstream.stop_jack(&jack_name).await;
            }
        }
        self.sessions.broadcast_tools_list_changed().await;
        crate::utils::request_log::log_event(
            self,
            &format!("custom_enable '{}'", client_name),
        );
        // (S13 W-D4) Fan out to EVERY interface from the mutator itself, so
        // a future caller cannot forget.
        // The "Custom [n/m]" submenu LABEL changes, so this is structural.
        self.notify_structure_changed();
        Ok(())
    }

    // ---- S10c: first-connection approval gate + forbidden clients -----------
    //
    // `ensure_client_approved` is the gate in front of `record_seen_client`: for
    // a NEW identity (not in seen_clients, not forbidden) with the gate ON, it
    // blocks the `initialize` request on a native Win32 dialog until the user
    // answers. The dialog itself runs on a plain std thread (MessageBoxW blocks)
    // and signals the result back via a watch channel. Concurrent initialize
    // retries for the SAME not-yet-decided identity JOIN the same pending
    // decision (subscribe) instead of popping a second dialog. The decision is
    // APPLIED by `apply_approval_decision`; un-forbidding a client later is
    // `remove_forbidden_client`.

    /// First-connection approval gate (S10c). Called from `handle_initialize`
    /// AFTER the client identity is resolved (header priority). `version` is the
    /// connecting agent's `clientInfo.version` (display-only), threaded in so an
    /// Allow decision records it. Fast paths (no dialog needed):
    /// - the gate is OFF (`require_approval_for_new_clients == false`) -> the
    ///   identity is auto-added to `seen_clients` immediately (unchanged S10
    ///   behavior) via [`Self::record_seen_client`];
    /// - the identity is already in `seen_clients` (a prior Allow stands);
    /// - the identity is already in `forbidden_clients` (a prior Deny stands).
    ///
    /// Otherwise the FIRST requester for an identity spawns the blocking dialog
    /// thread and awaits its decision; concurrent requesters for the same
    /// not-yet-decided identity subscribe to the SAME decision (no second
    /// dialog). Resolves when the user has answered (the `initialize` request
    /// stays pending for that one async task; other clients are unaffected).
    ///
    /// On Allow the thread records the client in `seen_clients` (atomically, with
    /// the version) so a concurrent retry immediately sees it as "known" (closing
    /// the race between signaling Allow and the next `initialize`). On Deny the
    /// identity lands in `forbidden_clients` — the agent gets zero tools but
    /// `initialize` still completes (no transport-level failure).
    pub async fn ensure_client_approved(&self, name: &str, version: Option<&str>) {
        if name.is_empty() {
            return;
        }

        // Fast path: decide whether a dialog is needed at all, under one config
        // read. Gate OFF, or the identity is already known/forbidden -> no
        // dialog (a previous decision stands). Gate OFF additionally auto-adds
        // the client to seen_clients (today's S10 behavior).
        let (gate_on, known, forbidden) = {
            let cfg = self.config.read();
            (
                cfg.require_approval_for_new_clients,
                cfg.seen_clients.iter().any(|c| c.name == name),
                cfg.is_forbidden(Some(name)),
            )
        };
        if !gate_on {
            // Gate OFF + not yet known + not forbidden -> auto-record (no dialog).
            if !known && !forbidden {
                self.record_seen_client(name, version);
            }
            return;
        }
        if known || forbidden {
            return;
        }

        // Slow path: a dialog IS needed. Either join an already-pending decision
        // for the SAME identity (concurrent initialize retries), or become the
        // first requester and spawn the blocking dialog thread. The lock is held
        // only for the map bookkeeping (insert / subscribe), never across the
        // await below.
        let rx: tokio::sync::watch::Receiver<Option<bool>> = {
            let mut pending = self.pending_approvals.lock();
            if let Some(tx) = pending.get(name) {
                // A dialog for this identity is already up — do NOT pop a second
                // one. Join the same pending decision (watch receivers are
                // cheaply cloneable via subscribe()).
                tx.subscribe()
            } else {
                // First requester: create the channel, register it, and spawn the
                // blocking dialog on a plain std thread (NOT the tokio runtime —
                // MessageBoxW runs its own modal loop and would stall an async
                // worker). The thread owns applying the decision + signaling
                // every awaiter, so a cancelled initialize task cannot strand
                // the decision.
                let (tx, rx) = tokio::sync::watch::channel(None::<bool>);
                pending.insert(name.to_string(), tx);
                let state2 = self.clone();
                let name_owned = name.to_string();
                let version_owned = version.map(str::to_owned);
                let handle_for_dialog = self.tray_handle.read().clone();
                std::thread::spawn(move || {
                    // (S13 §4.7) The popover is always-on-top; a modal Win32
                    // dialog raised underneath it looks like a frozen machine.
                    // Step it aside for the duration of the prompt.
                    if let Some(h) = &handle_for_dialog {
                        crate::window::set_yielding(h, true);
                    }
                    let allowed = crate::approval::show_approval_dialog(&name_owned);
                    if let Some(h) = &handle_for_dialog {
                        crate::window::set_yielding(h, false);
                    }
                    state2.apply_approval_decision(&name_owned, version_owned.as_deref(), allowed);
                });
                rx
            }
        };

        // Wait for the thread to signal the decision (Some(allowed)). Concurrent
        // joiners and the first requester all end up here. watch::wait_for
        // returns as soon as the current value satisfies the predicate; after
        // `send` the value is Some and this resolves immediately.
        let mut rx = rx;
        if rx.wait_for(|v| v.is_some()).await.is_err() {
            // All senders dropped without a value (shouldn't happen: the thread
            // always sends before removing the sender). Fall through leniently —
            // a not-forbidden identity still completes initialize.
            log("ensure_client_approved: decision channel closed without a value");
        }
    }

    /// Apply a first-connection approval decision (S10c). Called from the std
    /// thread that ran the dialog (so it must be safe off the tokio runtime —
    /// parking_lot locks + plain `config::save` are). This is the testable
    /// DECISION-LOGIC half, separated from the blocking `show_approval_dialog`
    /// side effect.
    ///
    /// - `allowed == true`: adds the identity to `seen_clients` (idempotent, with
    ///   `version`) so a concurrent initialize retry immediately sees it as
    ///   "known" (closing the race between signaling Allow and the next request),
    ///   AND defensively clears any stale `forbidden_clients` entry for it.
    /// - `allowed == false`: appends the identity to `forbidden_clients`
    ///   (idempotent) and NEVER to `seen_clients`.
    ///
    /// Then signals every awaiter of the decision (first requester + concurrent
    /// joiners) and clears the pending entry so the next new identity (or a
    /// reconnect after a later un-forbid) gets a fresh dialog. Persists + (when
    /// the config changed) rebuilds the tray so the "Custom (N)" / "Forbidden
    /// (N)" counts are live.
    fn apply_approval_decision(&self, name: &str, version: Option<&str>, allowed: bool) {
        // 1. Build a candidate snapshot from a READ (no mutation of the live
        //    config yet), matching the save-then-commit discipline used by
        //    add_jack/remove_jack/set_forbidden: persist FIRST, only commit to
        //    the live config if the disk write actually succeeds, so a failed
        //    `persist` can never leave memory and disk diverged. On
        //    Allow: record seen_clients (with version) + clear stale
        //    forbidden. On Deny: add to forbidden.
        let (candidate, changed) = {
            let cfg = self.config.read();
            let mut candidate = cfg.clone();
            let changed = if allowed {
                let mut changed = false;
                if !candidate.seen_clients.iter().any(|c| c.name == name) {
                    let now = chrono::Local::now().to_rfc3339();
                    candidate.seen_clients.push(crate::config::SeenClient {
                        name: name.to_string(),
                        first_seen_version: version.map(str::to_owned),
                        first_seen: now.clone(),
                        last_seen: Some(now),
                    });
                    changed = true;
                }
                let before = candidate.forbidden_clients.len();
                candidate.forbidden_clients.retain(|c| c != name);
                if candidate.forbidden_clients.len() != before {
                    changed = true;
                }
                changed
            } else if !candidate.forbidden_clients.iter().any(|c| c == name) {
                candidate.forbidden_clients.push(name.to_string());
                true
            } else {
                false
            };
            (candidate, changed)
        };
        if changed {
            if let Err(e) = self.persist(&candidate) {
                log(&format!("apply_approval_decision: failed to persist: {}", e));
                // Fail CLOSED, not open: enforcement here is security-relevant
                // (a Deny must actually block the session `ensure_client_approved`
                // is about to let through), unlike the pure-consistency mutators
                // elsewhere that leave memory untouched on a failed save. If we
                // left memory unmutated here, a disk write failure during a Deny
                // would silently let the denied identity in for this run (it
                // would only become forbidden after a LATER successful save) —
                // worse than the memory/disk divergence this commits instead,
                // which self-heals on the next successful save.
                if !allowed {
                    *self.config.write() = candidate;
                }
            } else {
                *self.config.write() = candidate;
            }
        }

        // 2. Signal every awaiter + clear the pending entry. Sending BEFORE the
        //    sender is dropped (the remove takes ownership) means receivers that
        //    have not yet polled still observe Some(allowed) via the retained
        //    last value.
        {
            let mut pending = self.pending_approvals.lock();
            if let Some(tx) = pending.remove(name) {
                let _ = tx.send(Some(allowed));
            }
        }

        // 3. Rebuild the tray menu so "Custom (N)" (Allow added a seen client)
        //    / "Forbidden (N)" (Deny) reflect the decision live. Gated on
        //    `changed` so a no-op decision doesn't pointlessly rebuild.
        if changed {
            // (S13) One fan-out for both interfaces.
            self.notify_structure_changed();

            #[cfg(not(test))]
            {
                let state2 = self.clone();
                tauri::async_runtime::spawn(async move {
                    state2.reconcile_all_jack_lifecycles().await;
                });
            }
        }

        log(&format!(
            "apply_approval_decision: '{}' {}",
            name,
            if allowed { "allowed -> seen" } else { "denied -> forbidden" }
        ));
    }


    /// Toggle whether a client identity is forbidden (S11 tray "Forbidden"
    /// submenu redesign). `forbidden == true` appends the identity to
    /// `forbidden_clients` (idempotent); `forbidden == false` removes it
    /// (idempotent), a symmetric toggle. Does NOT touch `seen_clients` in either
    /// direction (an identity toggled here was necessarily already seen to
    /// appear in the list at all). Persists via the save-then-commit discipline
    /// (read → mutate a snapshot → `config::save` → commit to live config) used
    /// by `add_jack`/`remove_jack`, then reconciles every shared child against
    /// `should_run_jack` (forbidding can drop the last Custom consumer of a
    /// globally-off jack; un-forbidding can re-enable one) and broadcasts
    /// `notifications/tools/list_changed`.
    pub async fn set_forbidden(&self, identity: &str, forbidden: bool) -> Result<(), String> {
        // Build the candidate snapshot from a READ (no mutation of the live
        // config yet), tracking whether it actually differs so a no-op toggle
        // skips the disk write + lifecycle reconcile.
        let (candidate, changed) = {
            let cfg = self.config.read();
            let mut snap = cfg.clone();
            let changed = if forbidden {
                let already = snap.forbidden_clients.iter().any(|c| c == identity);
                if !already {
                    snap.forbidden_clients.push(identity.to_string());
                }
                !already
            } else {
                let before = snap.forbidden_clients.len();
                snap.forbidden_clients.retain(|c| c != identity);
                before != snap.forbidden_clients.len()
            };
            (snap, changed)
        };
        if !changed {
            // No-op: still broadcast so any stale client state re-evaluates.
            self.sessions.broadcast_tools_list_changed().await;
            return Ok(());
        }
        // Save FIRST; only commit to the live config if the disk write succeeded
        // (so a failed save can never leave memory and disk diverged).
        if let Err(e) = self.persist(&candidate) {
            log(&format!("set_forbidden: failed to persist: {}", e));
            return Err(e);
        }
        *self.config.write() = candidate;
        // A forbidden change can flip should_run_jack for jacks this identity's
        // Custom override was uniquely keeping alive (forbid) or now re-enables
        // (un-forbid). reconcile_all_jack_lifecycles also broadcasts.
        self.reconcile_all_jack_lifecycles().await;
        crate::utils::request_log::log_event(
            self,
            &format!("set_forbidden '{}' -> forbidden={}", identity, forbidden),
        );
        // (S13 W-D4) Fan out to EVERY interface from the mutator itself, so
        // a future caller cannot forget.
        // The "Forbidden [n]" submenu LABEL changes, so this is structural.
        self.notify_structure_changed();
        Ok(())
    }

    /// Turn "Custom" mode OFF for a client (S11 explicit "Enable Custom
    /// permissions" checkbox). Sets `enabled = false` on the existing
    /// [`ClientOverride`] entry for `client_name` WITHOUT clearing its `jacks`
    /// map, so re-enabling later (via [`Self::enable_custom_client`]) restores
    /// the prior per-jack customization instead of starting over. If no
    /// `ClientOverride` entry exists at all, this is a harmless no-op. Persists
    /// via the save-then-commit discipline, then reconciles shared children
    /// (disabling an override can stop a jack this client was uniquely keeping
    /// alive via a globally-off Custom entry) and broadcasts.
    pub async fn disable_custom_client(&self, client_name: &str) -> Result<(), String> {
        // Build the candidate snapshot from a READ; only a currently-ENABLED
        // override entry represents real work (an absent or already-disabled
        // entry is a no-op).
        let (candidate, changed) = {
            let cfg = self.config.read();
            let mut snap = cfg.clone();
            let changed = match snap.client_overrides.get_mut(client_name) {
                Some(ovr) => {
                    if ovr.enabled {
                        ovr.enabled = false; // jacks map left intact
                        true
                    } else {
                        false // already disabled
                    }
                }
                None => false, // no override entry -> nothing to disable
            };
            (snap, changed)
        };
        if !changed {
            self.sessions.broadcast_tools_list_changed().await;
            return Ok(());
        }
        if let Err(e) = self.persist(&candidate) {
            log(&format!("disable_custom_client: failed to persist: {}", e));
            return Err(e);
        }
        *self.config.write() = candidate;
        self.reconcile_all_jack_lifecycles().await;
        crate::utils::request_log::log_event(
            self,
            &format!("custom_disable '{}'", client_name),
        );
        // (S13 W-D4) Fan out to EVERY interface from the mutator itself, so
        // a future caller cannot forget.
        self.notify_structure_changed();
        Ok(())
    }

    /// Permanently delete a known agent identity from Patchbay's memory (S12,
    /// tray "Custom" submenu → "✕ Delete this agent"). This is a PURGE, not a
    /// deny: the identity is removed from `seen_clients`, `client_overrides`,
    /// AND `forbidden_clients`. Unlike [`Self::disable_custom_client`] (which
    /// preserves the [`ClientOverride`] entry with `enabled: false`), this
    /// ERASES the entry; unlike [`Self::set_forbidden`] (which only denies
    /// access while keeping the identity known), this forgets it entirely. If
    /// the same identity connects again later, Patchbay treats it as a brand-
    /// new agent (fresh approval-gate prompt if the gate is on, fresh
    /// `seen_clients` entry).
    ///
    /// This method is the testable DECISION-LOGIC half (the blocking
    /// [`crate::approval::show_delete_confirm_dialog`] confirm dialog is
    /// live-verification-only and runs on a std thread before this is ever
    /// called). It follows the SAME save-then-commit discipline as
    /// [`Self::set_forbidden`]/[`Self::disable_custom_client`]: build a
    /// candidate snapshot from a READ, persist it FIRST, only then commit to
    /// the live config, THEN [`Self::reconcile_all_jack_lifecycles`] (removing
    /// a Custom override could have been the only thing keeping some jack's
    /// shared child alive) and broadcast `notifications/tools/list_changed`.
    /// If NONE of the three collections actually contained the identity, this
    /// is a harmless no-op (no disk write, no reconcile).
    pub async fn delete_client(&self, identity: &str) -> Result<(), String> {
        // Build the candidate snapshot from a READ (no mutation of the live
        // config yet), tracking whether ANY of the three collections actually
        // contained the identity so a no-op delete skips the disk write +
        // lifecycle reconcile.
        let (candidate, changed) = {
            let cfg = self.config.read();
            let mut snap = cfg.clone();
            let mut changed = false;

            // seen_clients: drop the matching entry by name.
            let before_seen = snap.seen_clients.len();
            snap.seen_clients.retain(|c| c.name != identity);
            if snap.seen_clients.len() != before_seen {
                changed = true;
            }

            // client_overrides: remove the entry ENTIRELY (unlike
            // disable_custom_client, which only flips enabled:false).
            if snap.client_overrides.remove(identity).is_some() {
                changed = true;
            }

            // forbidden_clients: drop the identity if it happened to be denied.
            let before_forbidden = snap.forbidden_clients.len();
            snap.forbidden_clients.retain(|c| c != identity);
            if snap.forbidden_clients.len() != before_forbidden {
                changed = true;
            }

            (snap, changed)
        };
        if !changed {
            // Unknown/never-seen identity: nothing to purge. No disk write, no
            // reconcile (and nothing about any client's view changed, so no
            // broadcast either).
            log(&format!("delete_client: '{}' not known — no-op", identity));
            return Ok(());
        }
        // Save FIRST; only commit to the live config if the disk write succeeded
        // (so a failed save can never leave memory and disk diverged).
        if let Err(e) = self.persist(&candidate) {
            log(&format!("delete_client: failed to persist: {}", e));
            return Err(e);
        }
        *self.config.write() = candidate;
        // Removing a Custom override could have been the only thing keeping some
        // globally-off jack's shared child alive — reconcile the same way
        // set_forbidden / disable_custom_client do (which also broadcasts).
        self.reconcile_all_jack_lifecycles().await;
        crate::utils::request_log::log_event(self, &format!("delete_client '{}'", identity));
        log(&format!("delete_client: '{}' purged from Patchbay", identity));
        // (S13 W-D4) Fan out to EVERY interface from the mutator itself, so
        // a future caller cannot forget.
        // The agent disappears from both submenus: structural.
        self.notify_structure_changed();
        Ok(())
    }

    /// Toggle the Level-2 request/event log on/off (tray "Enable request
    /// logging"). Save-then-commit discipline (same as add_jack /
    /// remove_jack / set_forbidden): build a candidate from a READ, persist it
    /// FIRST, and only commit to the live config if the disk write succeeded —
    /// a failed `config::save` leaves NEITHER disk NOR memory mutated, so the
    /// caller can reconcile the checkbox to the unchanged value. Pure logging
    /// flag: no upstream child or session side effects, so no broadcast.
    pub fn set_request_logging_enabled(&self, enabled: bool) -> Result<(), String> {
        let (candidate, changed) = {
            let cfg = self.config.read();
            let mut snap = cfg.clone();
            let changed = snap.request_logging_enabled != enabled;
            snap.request_logging_enabled = enabled;
            (snap, changed)
        };
        if !changed {
            return Ok(());
        }
        self.persist(&candidate)?;
        *self.config.write() = candidate;
        // (S13 W-D4) Fan out to EVERY interface from the mutator itself.
        self.notify_state_changed();
        Ok(())
    }

    /// Reconcile every configured shared child with `should_run_jack`.
    ///
    /// Used after approval decisions because forbidding an identity can remove
    /// the last Custom consumer of a globally-off jack, while allowing one can
    /// re-enable an existing Custom list. Starts complete before the broadcast
    /// so newly-visible tools have a cache; stops happen after the broadcast so
    /// newly-hidden tools disappear from clients before the child is torn down.
    async fn reconcile_all_jack_lifecycles(&self) {
        let jacks = self.config.read().jacks.clone();
        let mut stop_after_broadcast: Vec<String> = Vec::new();

        for jack in &jacks {
            let _guard = self.upstream.jack_lock(&jack.name).await;
            let should_run = self.config.read().should_run_jack(&jack.name);
            let is_running = self.upstream.is_jack_running(&jack.name);
            if should_run && !is_running {
                self.upstream
                    .start_jack(jack, self.sessions.clone(), self.config.clone())
                    .await;
            } else if !should_run && is_running {
                stop_after_broadcast.push(jack.name.clone());
            }
        }

        self.sessions.broadcast_tools_list_changed().await;

        for name in stop_after_broadcast {
            let _guard = self.upstream.jack_lock(&name).await;
            if !self.config.read().should_run_jack(&name) && self.upstream.is_jack_running(&name) {
                self.upstream.stop_jack(&name).await;
            }
        }
    }
}

/// Result of a toggle through [`AppState::set_patched`]: the resulting
/// `patched` flag (authoritative after the flip) + a status string the caller
/// surfaces in the UI (tray check reconcile / tooltip) or the debug response.
///
/// `Serialize` since S13: the window's `ui_toggle_jack` returns it straight to
/// the row that was clicked, so the switch reconciles against the AUTHORITATIVE
/// result — including a refused toggle, whose status names the reason.
#[derive(Clone, Debug, Serialize)]
pub struct ToggleResult {
    pub patched: bool,
    pub status: String,
}

/// One row of the tray's per-jack view (name / patched).
#[derive(Clone, Debug)]
pub struct JackLine {
    pub name: String,
    pub patched: bool,
}

// ---- S8: add/remove/list jack support types ------------------------------

/// Validation / persist failure for [`AppState::add_jack`].
#[derive(Clone, Debug)]
pub enum AddJackError {
    /// Name failed [`crate::config::is_valid_jack_name`] (bad charset, contains
    /// `__`, too long, or empty).
    InvalidName(String),
    /// A jack with this name already exists in the config.
    DuplicateName(String),
    /// The transport variant is missing its required field (stdio: `command`;
    /// streamable-http: `url`).
    MissingRequiredField(String),
    /// The config file could not be persisted.
    PersistFailed(String),
}

impl AddJackError {
    /// Human-readable reason (surfaced to the agent/script verbatim).
    pub fn message(&self) -> String {
        match self {
            AddJackError::InvalidName(n) => format!(
                "invalid jack name '{}': must match [A-Za-z0-9_-]+, contain no '__', and be <= 40 chars",
                n
            ),
            AddJackError::DuplicateName(n) => format!("a jack named '{}' already exists", n),
            AddJackError::MissingRequiredField(s) => s.clone(),
            AddJackError::PersistFailed(s) => format!("failed to persist config: {}", s),
        }
    }
}

/// Failure for [`AppState::remove_jack`].
#[derive(Clone, Debug)]
pub enum RemoveJackError {
    /// No jack with this name exists.
    NotFound(String),
    /// The config file could not be persisted.
    PersistFailed(String),
}

impl RemoveJackError {
    /// Human-readable reason.
    pub fn message(&self) -> String {
        match self {
            RemoveJackError::NotFound(n) => format!("jack '{}' not found", n),
            RemoveJackError::PersistFailed(s) => format!("failed to persist config: {}", s),
        }
    }
}

/// One jack's summary for list/add responses (S8). Serialized to JSON for the
/// admin HTTP endpoints and formatted to text for the meta MCP tools.
#[derive(Clone, Debug, Serialize)]
pub struct JackSummary {
    pub name: String,
    pub patched: bool,
    /// `"stdio"` or `"streamable_http"`.
    pub transport: String,
    /// Runtime status string (`running` / `starting` / `stopped` /
    /// `failed: <reason>` / `unknown`).
    pub status: String,
    /// Number of cached tools (0 unless patched AND running).
    pub tool_count: usize,
}

/// Transport type label for summaries: `"stdio"` / `"streamable_http"`.
///
/// Note: the on-disk config discriminator is kebab-case (`"streamable-http"`,
/// from [`JackTransport`]'s serde tags), but the summary uses underscore so it
/// reads cleanly as a stable API token (per the S8 spec).
fn transport_type_string(t: &JackTransport) -> String {
    match t {
        JackTransport::Stdio { .. } => "stdio".to_string(),
        JackTransport::StreamableHttp { .. } => "streamable_http".to_string(),
    }
}

/// Validate a transport's required field: stdio needs a non-empty `command`;
/// streamable-http needs a non-empty `url`. (This mirrors the checks inside
/// `upstream::start_jack`, performed up-front so a bad input is rejected before
/// the config is written.)
fn validate_transport(transport: &JackTransport) -> Result<(), AddJackError> {
    match transport {
        JackTransport::Stdio { command, .. } if command.is_empty() => Err(
            AddJackError::MissingRequiredField(
                "stdio transport requires a non-empty 'command'".to_string(),
            ),
        ),
        JackTransport::StreamableHttp { url, .. } if url.is_empty() => Err(
            AddJackError::MissingRequiredField(
                "streamable-http transport requires a non-empty 'url'".to_string(),
            ),
        ),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{first_run_template, JackTransport, Sharing};
    use std::collections::BTreeMap;

    fn stdio_input(name: &str, command: &str) -> JackConfigInput {
        JackConfigInput {
            name: name.to_string(),
            patched: true,
            transport: JackTransport::Stdio {
                command: command.to_string(),
                args: vec![],
                env: BTreeMap::new(),
            },
            sharing: Sharing::Shared,
        }
    }

    fn http_input(name: &str, url: &str) -> JackConfigInput {
        JackConfigInput {
            name: name.to_string(),
            patched: true,
            transport: JackTransport::StreamableHttp {
                url: url.to_string(),
                headers: BTreeMap::new(),
            },
            sharing: Sharing::Shared,
        }
    }

    fn state_with_prod() -> AppState {
        // first_run_template ships one jack "prod" (patched:false). No upstream
        // is ever started in these tests, so no child is spawned.
        AppState::new(first_run_template())
    }

    // ---- add_jack validation (all rejected BEFORE config::save, so the real
    // patchbay.json is never touched) ----

    #[tokio::test]
    async fn add_jack_rejects_duplicate_name() {
        let st = state_with_prod();
        let err = st.add_jack(stdio_input("prod", "npx")).await.unwrap_err();
        assert!(matches!(err, AddJackError::DuplicateName(_)), "{:?}", err);
        assert!(err.message().contains("already exists"));
    }

    #[tokio::test]
    async fn add_jack_rejects_invalid_name_charset() {
        let st = state_with_prod();
        let err = st
            .add_jack(stdio_input("bad name", "npx"))
            .await
            .unwrap_err();
        assert!(matches!(err, AddJackError::InvalidName(_)), "{:?}", err);
    }

    #[tokio::test]
    async fn add_jack_rejects_name_with_separator() {
        let st = state_with_prod();
        let err = st.add_jack(stdio_input("a__b", "npx")).await.unwrap_err();
        assert!(matches!(err, AddJackError::InvalidName(_)), "{:?}", err);
    }

    #[tokio::test]
    async fn add_jack_rejects_empty_command_for_stdio() {
        let st = state_with_prod();
        let err = st.add_jack(stdio_input("newjack", "")).await.unwrap_err();
        assert!(
            matches!(err, AddJackError::MissingRequiredField(_)),
            "{:?}",
            err
        );
        assert!(err.message().contains("command"));
    }

    #[tokio::test]
    async fn add_jack_rejects_empty_url_for_http() {
        let st = state_with_prod();
        let err = st.add_jack(http_input("newjack", "")).await.unwrap_err();
        assert!(
            matches!(err, AddJackError::MissingRequiredField(_)),
            "{:?}",
            err
        );
        assert!(err.message().contains("url"));
    }

    // ---- remove_jack validation ----

    #[tokio::test]
    async fn remove_jack_returns_not_found_for_unknown() {
        let st = state_with_prod();
        let err = st.remove_jack("ghost").await.unwrap_err();
        assert!(matches!(err, RemoveJackError::NotFound(_)), "{:?}", err);
        assert!(err.message().contains("ghost"));
    }

    // ---- list_jacks (no disk, no spawn) ----

    #[test]
    fn list_jacks_reflects_config() {
        let st = state_with_prod();
        let jacks = st.list_jacks();
        assert_eq!(jacks.len(), 1);
        let j = &jacks[0];
        assert_eq!(j.name, "prod");
        assert!(!j.patched); // first_run_template ships prod off
        assert_eq!(j.transport, "stdio");
        // Never started -> runtime absent -> status "unknown".
        assert_eq!(j.status, "unknown");
        assert_eq!(j.tool_count, 0);
    }

    #[test]
    fn list_jacks_reports_http_transport_type() {
        let mut cfg = first_run_template();
        cfg.jacks[0].transport = JackTransport::StreamableHttp {
            url: "https://example.com/mcp".to_string(),
            headers: BTreeMap::new(),
        };
        let st = AppState::new(cfg);
        let jacks = st.list_jacks();
        assert_eq!(jacks[0].transport, "streamable_http");
    }

    #[test]
    fn transport_type_string_labels() {
        assert_eq!(
            transport_type_string(&JackTransport::Stdio {
                command: String::new(),
                args: vec![],
                env: BTreeMap::new(),
            }),
            "stdio"
        );
        assert_eq!(
            transport_type_string(&JackTransport::StreamableHttp {
                url: String::new(),
                headers: BTreeMap::new(),
            }),
            "streamable_http"
        );
    }

    // ---- S10: set_client_override / enable_custom_client / sync-on-add ----

    /// Route `config::save` (called by the state-mutating S10 methods) at a
    /// unique temp path so these tests never touch the real
    /// `%APPDATA%\Patchbay\patchbay.json`. Assertions read the in-memory
    /// `AppState::config`, so a unique path per call also avoids any cross-test
    /// rename races on a shared file.
    fn isolate_config() {
        config::set_test_config_path(Some(config::fresh_test_config_path()));
    }

    /// A stdio add input with a CHOSEN patched flag (so the propagation test can
    /// add a jack OFF and avoid spawning a real `npx` child in the test).
    fn stdio_input_patched(name: &str, patched: bool) -> JackConfigInput {
        JackConfigInput {
            name: name.to_string(),
            patched,
            transport: JackTransport::Stdio {
                command: "npx".to_string(),
                args: vec![],
                env: BTreeMap::new(),
            },
            sharing: Sharing::Shared,
        }
    }

    // ---- B-1: a corrupt config on disk must survive the app running -------

    #[test]
    fn a_corrupt_config_is_never_overwritten_by_a_background_mutator() {
        // Reproduces the real sequence: the file on disk fails to parse, so
        // `main` puts a safe_default in memory and records the reason. The app
        // keeps running, an agent connects, and a routine background save
        // fires. Before the guard, that save wrote the empty default over the
        // user's real file — every jack and every encrypted secret — within
        // about a minute of startup, with nothing to restore from.
        let path = crate::config::fresh_test_config_path();
        let corrupt = "{ this is not json";
        std::fs::write(&path, corrupt).expect("write corrupt");
        crate::config::set_test_config_path(Some(path.clone()));

        let st = state_two_jacks();
        *st.config_error.write() = Some("parse error".to_string());

        // Every shape of mutator: one that reports its failure, and one that
        // swallows it (the dangerous kind — nobody is watching its return).
        let err = st
            .set_ui_mode(crate::config::UiMode::Window)
            .expect_err("a save must be refused while the config is corrupt");
        assert!(err.contains("did not parse"), "unhelpful message: {}", err);
        st.set_autostart(true).expect_err("also refused");
        // The dangerous shape: a mutator whose failure nobody looks at. It must
        // still leave the file alone.
        st.touch_client_last_seen("codex");
        let _ = st.set_request_logging_enabled(true);

        assert_eq!(
            std::fs::read_to_string(&path).expect("read back"),
            corrupt,
            "the corrupt file was modified — the user's config is what was lost"
        );

        // And the guard lifts the moment the file is known-good again, or the
        // app would be permanently read-only after one bad parse.
        *st.config_error.write() = None;
        st.set_ui_mode(crate::config::UiMode::Window)
            .expect("saving must work again once the error is cleared");
        crate::config::set_test_config_path(None);
    }

    #[test]
    fn agent_traffic_never_rewrites_the_config_file() {
        // The whole point of the S14 split. An agent connecting, and its
        // once-a-minute liveness refresh, must leave `patchbay.json` byte-for-
        // byte alone: that file holds every server definition and every
        // DPAPI-encrypted secret, and rewriting it several times an hour to
        // record something nobody configured was pure risk for no benefit.
        let path = crate::config::fresh_test_config_path();
        crate::config::set_test_config_path(Some(path.clone()));

        let st = state_two_jacks();
        st.set_require_approval(false).expect("gate off for this test");
        let before = std::fs::read_to_string(&path).expect("the config must exist by now");
        let before_mtime = std::fs::metadata(&path).unwrap().modified().unwrap();

        st.record_seen_client("Codex", Some("1.0"));
        // The refresh is throttled per client, so drive it past the throttle
        // rather than pretending one call proves anything.
        st.last_seen_touch.lock().clear();
        st.touch_client_last_seen("Codex");

        assert_eq!(
            std::fs::read_to_string(&path).expect("read back"),
            before,
            "agent traffic rewrote the config file"
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            before_mtime,
            "the config file was rewritten with identical contents - still a              chance to lose it, and still a lie in the file's timestamp"
        );

        // ...and it did land somewhere: the observation must survive a reload,
        // or this test would pass equally well if nothing were persisted.
        let state_path = crate::config::state_path_for(&path);
        assert!(state_path.exists(), "nothing was written to the state file");
        let reloaded = crate::config::load_from_path(&path).expect("reload");
        let codex = reloaded
            .seen_clients
            .iter()
            .find(|c| c.name == "Codex")
            .expect("the agent was not recorded anywhere");
        assert!(codex.last_seen.is_some(), "last_seen was not persisted");
        crate::config::set_test_config_path(None);
    }

    #[test]
    fn nothing_writes_the_config_except_the_guarded_helper() {
        // The guard is only worth having if it cannot be walked around. A new
        // mutator that calls config::save directly would reintroduce the whole
        // bug silently, so the ban is checked mechanically rather than
        // remembered.
        let src = include_str!("app_state.rs");
        // Assembled from pieces so this line is not itself a hit: the whole
        // token never appears in the file, only in the string it builds.
        let needles = [
            concat!("config::", "save("),
            concat!("config::", "save_runtime_state("),
        ];
        // The two guard bodies are the only places allowed to call through.
        // Spelled out rather than located by line number so that moving them
        // does not silently widen the exemption. Both these literals and the
        // needles above are assembled from pieces, so this test's own source
        // is not a hit.
        let allowed = [
            concat!("config::", "save(candidate)"),
            concat!("config::", "save_runtime_state(candidate)"),
        ];
        let direct: Vec<&str> = src
            .lines()
            .filter(|l| needles.iter().any(|n| l.contains(n)))
            .filter(|l| !allowed.contains(&l.trim()))
            .collect();
        assert!(
            direct.is_empty(),
            "these lines bypass AppState::persist and can overwrite a corrupt \
             config with an empty default: {:?}",
            direct
        );
    }

    fn state_two_jacks() -> AppState {
        // `alpha` patched ON, `beta` patched OFF globally.
        let cfg = crate::config::PatchbayConfig {
            version: crate::config::CURRENT_VERSION,
            port: crate::config::DEFAULT_PORT,
            autostart: false,
            ui_mode: crate::config::UiMode::Tray,
            jacks: vec![
                JackConfig {
                    name: "alpha".to_string(),
                    patched: true,
                    transport: JackTransport::Stdio {
                        command: String::new(),
                        args: vec![],
                        env: BTreeMap::new(),
                    },
                    sharing: Sharing::Shared,
                    tools: None,
                },
                JackConfig {
                    name: "beta".to_string(),
                    patched: false,
                    transport: JackTransport::Stdio {
                        command: String::new(),
                        args: vec![],
                        env: BTreeMap::new(),
                    },
                    sharing: Sharing::Shared,
                    tools: None,
                },
            ],
            bays: BTreeMap::new(),
            seen_clients: Vec::new(),
            client_overrides: BTreeMap::new(),
            require_approval_for_new_clients: true,
            request_logging_enabled: false,
            forbidden_clients: Vec::new(),
        };
        AppState::new(cfg)
    }

    #[tokio::test]
    async fn set_client_override_lazily_creates_full_snapshot() {
        isolate_config();
        // First customization of a client: the override is created enabled with
        // a FULL copy of the current global list, then the one jack is flipped.
        let st = state_two_jacks();
        // Before: client "codex" inherits global (alpha on, beta off).
        assert!(st.config.read().effective_patched("alpha", Some("codex")));
        assert!(!st.config.read().effective_patched("beta", Some("codex")));

        // Flip alpha OFF for codex (lazily creates the override).
        st.set_client_override("codex", "alpha", false).await.unwrap();

        let cfg = st.config.read();
        let ovr = cfg.client_overrides.get("codex").expect("override created");
        assert!(ovr.enabled, "lazy create enables Custom mode");
        // Full snapshot seeded from global: alpha + beta both present.
        assert_eq!(ovr.jacks.len(), 2);
        assert_eq!(ovr.jacks.get("alpha"), Some(&false), "alpha flipped off");
        assert_eq!(
            ovr.jacks.get("beta"),
            Some(&false),
            "beta seeded from global (off)"
        );
        drop(cfg);

        // effective_patched now reflects codex's own list for alpha/beta.
        assert!(!st.config.read().effective_patched("alpha", Some("codex")));
        assert!(!st.config.read().effective_patched("beta", Some("codex")));
        // A DIFFERENT client still inherits global.
        assert!(st.config.read().effective_patched("alpha", Some("other")));
    }

    #[tokio::test]
    async fn set_client_override_second_jack_does_not_reseed() {
        isolate_config();
        // Once the override exists, flipping a second jack must NOT reseed the
        // whole list (would clobber the first flip).
        let st = state_two_jacks();
        st.set_client_override("codex", "alpha", false).await.unwrap();
        st.set_client_override("codex", "beta", true).await.unwrap();

        let cfg = st.config.read();
        let ovr = cfg.client_overrides.get("codex").unwrap();
        assert_eq!(ovr.jacks.get("alpha"), Some(&false), "first flip preserved");
        assert_eq!(ovr.jacks.get("beta"), Some(&true), "second flip applied");
    }

    #[tokio::test]
    async fn set_client_override_rejects_unknown_jack() {
        isolate_config();
        let st = state_two_jacks();
        let err = st
            .set_client_override("codex", "ghost", true)
            .await
            .unwrap_err();
        assert!(err.contains("not found"), "err: {}", err);
        assert!(
            !st.config.read().client_overrides.contains_key("codex"),
            "a stale tray click must not create a ghost override entry"
        );
    }

    #[tokio::test]
    async fn enable_custom_client_seeds_snapshot_no_flip() {
        isolate_config();
        let st = state_two_jacks();
        st.enable_custom_client("codex").await.unwrap();
        let cfg = st.config.read();
        let ovr = cfg.client_overrides.get("codex").unwrap();
        assert!(ovr.enabled);
        // Seeded from global: alpha on, beta off (NO flip).
        assert_eq!(ovr.jacks.get("alpha"), Some(&true));
        assert_eq!(ovr.jacks.get("beta"), Some(&false));
        // effective_patched unchanged from global at this point.
        assert!(cfg.effective_patched("alpha", Some("codex")));
        assert!(!cfg.effective_patched("beta", Some("codex")));
    }

    #[tokio::test]
    async fn add_jack_propagates_new_jack_into_custom_lists() {
        isolate_config();
        // A Custom client's list must gain a newly-added global jack (seeded with
        // the value it was created with). Added OFF so no real child is spawned.
        let st = state_two_jacks();
        st.set_client_override("codex", "alpha", false).await.unwrap();
        st.add_jack(stdio_input_patched("gamma", false)).await.unwrap();

        let cfg = st.config.read();
        let ovr = cfg.client_overrides.get("codex").unwrap();
        // gamma seeded with its created value (false) into codex's list.
        assert_eq!(ovr.jacks.get("gamma"), Some(&false));
    }

    #[tokio::test]
    async fn remove_jack_removes_jack_from_custom_lists() {
        isolate_config();
        let st = state_two_jacks();
        st.set_client_override("codex", "alpha", false).await.unwrap();
        st.remove_jack("alpha").await.unwrap();
        let cfg = st.config.read();
        let ovr = cfg.client_overrides.get("codex").unwrap();
        assert!(
            !ovr.jacks.contains_key("alpha"),
            "removed jack gone from custom list"
        );
    }

    #[test]
    fn record_seen_client_appends_only_once() {
        isolate_config();
        // record_seen_client is sync; it persists (best-effort) and appends. A
        // repeat of the same name is a no-op.
        let st = state_two_jacks();
        st.record_seen_client("claude-code", Some("1.0.0"));
        {
            let cfg = st.config.read();
            assert_eq!(cfg.seen_clients.len(), 1);
            assert_eq!(cfg.seen_clients[0].name, "claude-code");
            assert_eq!(
                cfg.seen_clients[0].first_seen_version.as_deref(),
                Some("1.0.0")
            );
        }
        // Repeated name: not duplicated.
        st.record_seen_client("claude-code", Some("2.0.0"));
        assert_eq!(st.config.read().seen_clients.len(), 1);
        // A different name appends.
        st.record_seen_client("codex", None);
        let cfg = st.config.read();
        assert_eq!(cfg.seen_clients.len(), 2);
    }

    // ---- S10c: approval decision logic + forbidden clients ----
    //
    // The blocking `show_approval_dialog` itself can't be unit-tested without a
    // live Windows session; the DECISION LOGIC it feeds (allow/deny → config;
    // the fast-path gate; un-forbid) is exercised here. `apply_approval_decision`
    // is called directly (the same function the dialog thread calls).

    #[tokio::test]
    async fn ensure_client_approved_fast_paths_skip_the_dialog() {
        isolate_config();

        // Gate OFF + new identity -> auto-adds to seen_clients (today's S10
        // behavior), no dialog, nothing forbidden.
        let st = state_two_jacks();
        st.config.write().require_approval_for_new_clients = false;
        st.ensure_client_approved("brand-new", None).await;
        assert!(
            st.config.read().seen_clients.iter().any(|c| c.name == "brand-new"),
            "gate OFF auto-records the client"
        );
        assert!(st.config.read().forbidden_clients.is_empty());

        // Gate ON but identity already seen -> no dialog, no new entry.
        let st2 = state_two_jacks();
        st2.record_seen_client("known", None);
        let before = st2.config.read().seen_clients.len();
        st2.ensure_client_approved("known", None).await;
        assert_eq!(st2.config.read().seen_clients.len(), before);

        // Gate ON but identity already forbidden -> no dialog, stays forbidden
        // and is NOT recorded as seen.
        let st3 = state_two_jacks();
        st3.config.write().forbidden_clients.push("banned".to_string());
        st3.ensure_client_approved("banned", None).await;
        assert!(st3.config.read().is_forbidden(Some("banned")));
        assert!(
            !st3.config.read().seen_clients.iter().any(|c| c.name == "banned"),
            "a forbidden identity is not recorded as seen"
        );
    }

    #[test]
    fn apply_approval_decision_deny_adds_to_forbidden_not_seen() {
        isolate_config();
        let st = state_two_jacks();
        // Apply a Deny decision directly (the decision logic, not the dialog).
        st.apply_approval_decision("rogue", None, false);
        let cfg = st.config.read();
        assert!(cfg.is_forbidden(Some("rogue")), "denied -> forbidden");
        assert!(
            !cfg.seen_clients.iter().any(|c| c.name == "rogue"),
            "a denied agent must NOT land in seen_clients"
        );
    }

    #[test]
    fn apply_approval_decision_allow_adds_to_seen_and_clears_forbidden() {
        isolate_config();
        let st = state_two_jacks();
        // Pre-seed a stale forbidden entry; an Allow decision must clear it AND
        // record the identity in seen_clients (so a concurrent retry sees it as
        // known), carrying the version.
        st.config.write().forbidden_clients.push("maybe".to_string());
        st.apply_approval_decision("maybe", Some("1.2.3"), true);
        let cfg = st.config.read();
        assert!(!cfg.is_forbidden(Some("maybe")));
        let maybe = cfg.seen_clients.iter().find(|c| c.name == "maybe").unwrap();
        assert_eq!(maybe.first_seen_version.as_deref(), Some("1.2.3"));
        drop(cfg);

        // A clean Allow adds to seen with the given version, forbids nothing.
        st.apply_approval_decision("fresh", None, true);
        let cfg = st.config.read();
        assert!(!cfg.is_forbidden(Some("fresh")));
        let fresh = cfg.seen_clients.iter().find(|c| c.name == "fresh").unwrap();
        assert_eq!(fresh.first_seen_version.as_deref(), None);
    }

    #[tokio::test]
    async fn remove_forbidden_client_removes_without_granting_seen() {
        isolate_config();
        let st = state_two_jacks();
        st.config.write().forbidden_clients.push("banned".to_string());
        st.set_forbidden("banned", false).await.unwrap();
        assert!(!st.config.read().is_forbidden(Some("banned")));
        // Un-forbidding does NOT retroactively mark the client as seen: a
        // reconnect goes through the gate again (unless the gate is off).
        assert!(
            !st.config.read().seen_clients.iter().any(|c| c.name == "banned"),
            "un-forbid must not grant seen_clients status"
        );
    }

    // ---- S11: set_forbidden (symmetric toggle) + disable_custom_client ----

    #[tokio::test]
    async fn set_forbidden_round_trip_without_touching_seen() {
        isolate_config();
        let st = state_two_jacks();
        // Seed seen_clients so the identity is "known" (as it would be to
        // appear in the Forbidden list at all).
        st.record_seen_client("codex", None);
        let seen_before = st.config.read().seen_clients.len();

        // Forbid: lands in forbidden_clients, idempotent on repeat.
        st.set_forbidden("codex", true).await.unwrap();
        assert!(st.config.read().is_forbidden(Some("codex")));
        st.set_forbidden("codex", true).await.unwrap(); // idempotent
        assert_eq!(
            st.config
                .read()
                .forbidden_clients
                .iter()
                .filter(|c| c == &"codex")
                .count(),
            1,
            "forbidding twice must not duplicate"
        );

        // Un-forbid: removed, idempotent on repeat.
        st.set_forbidden("codex", false).await.unwrap();
        assert!(!st.config.read().is_forbidden(Some("codex")));
        st.set_forbidden("codex", false).await.unwrap(); // idempotent no-op
        assert!(!st.config.read().is_forbidden(Some("codex")));

        // seen_clients is untouched in both directions.
        assert_eq!(
            st.config.read().seen_clients.len(),
            seen_before,
            "set_forbidden must not touch seen_clients"
        );
    }

    #[tokio::test]
    async fn set_forbidden_reflects_in_effective_patched() {
        isolate_config();
        let st = state_two_jacks();
        st.record_seen_client("codex", None);
        // alpha is globally ON; codex sees it before being forbidden.
        assert!(st.config.read().effective_patched("alpha", Some("codex")));
        st.set_forbidden("codex", true).await.unwrap();
        // Forbidden gate takes precedence: codex now sees no jacks.
        assert!(!st.config.read().effective_patched("alpha", Some("codex")));
        // A different client is unaffected.
        assert!(st.config.read().effective_patched("alpha", Some("other")));
    }

    #[tokio::test]
    async fn disable_custom_client_flips_enabled_preserves_jacks() {
        isolate_config();
        let st = state_two_jacks();
        // Enable Custom + flip alpha OFF for codex (so the jacks map is
        // customized and distinguishable from a fresh global seed).
        st.enable_custom_client("codex").await.unwrap();
        st.set_client_override("codex", "alpha", false).await.unwrap();
        {
            let cfg = st.config.read();
            let ovr = cfg.client_overrides.get("codex").unwrap();
            assert!(ovr.enabled);
            assert_eq!(ovr.jacks.get("alpha"), Some(&false), "alpha customized off");
            assert_eq!(ovr.jacks.get("beta"), Some(&false), "beta seeded from global");
        }
        // While Custom is enabled, codex's effective alpha is its own (false).
        assert!(!st.config.read().effective_patched("alpha", Some("codex")));

        // Disable Custom: enabled flips false, jacks map preserved.
        st.disable_custom_client("codex").await.unwrap();
        {
            let cfg = st.config.read();
            let ovr = cfg.client_overrides.get("codex").unwrap();
            assert!(!ovr.enabled, "disabled");
            assert_eq!(ovr.jacks.get("alpha"), Some(&false), "alpha customization preserved");
            assert_eq!(ovr.jacks.get("beta"), Some(&false), "beta customization preserved");
            assert_eq!(ovr.jacks.len(), 2, "jacks map intact (not cleared)");
        }
        // effective_patched falls back to global (alpha ON globally).
        assert!(
            st.config.read().effective_patched("alpha", Some("codex")),
            "disabled override falls back to global"
        );
    }

    #[tokio::test]
    async fn disable_custom_client_is_noop_without_override() {
        isolate_config();
        let st = state_two_jacks();
        // No override entry exists -> harmless no-op, creates nothing.
        st.disable_custom_client("ghost").await.unwrap();
        assert!(
            !st.config.read().client_overrides.contains_key("ghost"),
            "disabling a never-customized client must not create an entry"
        );
        // Idempotent on an already-disabled entry: still no-op.
        st.enable_custom_client("codex").await.unwrap();
        st.disable_custom_client("codex").await.unwrap();
        st.disable_custom_client("codex").await.unwrap();
        let cfg = st.config.read();
        let ovr = cfg.client_overrides.get("codex").unwrap();
        assert!(!ovr.enabled, "still disabled after double-disable");
    }

    #[tokio::test]
    async fn re_enable_after_disable_restores_same_jacks_map() {
        isolate_config();
        let st = state_two_jacks();
        // Enable + customize (alpha off, beta on — BOTH differ from global
        // alpha=on/beta=off, so a re-seed from global would be detectable).
        st.enable_custom_client("codex").await.unwrap();
        st.set_client_override("codex", "alpha", false).await.unwrap();
        st.set_client_override("codex", "beta", true).await.unwrap();

        // Disable, then re-enable.
        st.disable_custom_client("codex").await.unwrap();
        st.enable_custom_client("codex").await.unwrap();

        let cfg = st.config.read();
        let ovr = cfg.client_overrides.get("codex").unwrap();
        assert!(ovr.enabled, "re-enabled");
        // The SAME prior customization must survive the disable→enable round
        // trip (enable_custom_client's "already exists → just flip enabled"
        // branch must NOT re-seed over the preserved jacks map).
        assert_eq!(ovr.jacks.get("alpha"), Some(&false), "alpha customization restored");
        assert_eq!(ovr.jacks.get("beta"), Some(&true), "beta customization restored");
        // And effective_patched reflects the restored Custom list (not global).
        assert!(!cfg.effective_patched("alpha", Some("codex")));
        assert!(cfg.effective_patched("beta", Some("codex")));
    }

    // ---- S12: delete_client (purge a known agent entirely) ----
    //
    // delete_client removes an identity from seen_clients AND client_overrides
    // AND forbidden_clients, so a reconnected identity is treated as brand new.
    // Unlike disable_custom_client (which preserves the ClientOverride entry
    // with enabled:false), delete_client ERASES the entry. The blocking
    // `show_delete_confirm_dialog` is live-verification-only (it can't be unit-
    // tested without a live Windows session — same carve-out as
    // `show_approval_dialog`); the DECISION LOGIC here is what's under test.

    #[tokio::test]
    async fn delete_client_removes_from_seen_clients() {
        isolate_config();
        let st = state_two_jacks();
        st.record_seen_client("codex", None);
        st.record_seen_client("other", None);
        assert_eq!(st.config.read().seen_clients.len(), 2);

        st.delete_client("codex").await.unwrap();

        let cfg = st.config.read();
        assert!(
            !cfg.seen_clients.iter().any(|c| c.name == "codex"),
            "deleted identity gone from seen_clients"
        );
        assert!(
            cfg.seen_clients.iter().any(|c| c.name == "other"),
            "unrelated identity untouched"
        );
    }

    #[tokio::test]
    async fn delete_client_erases_custom_override_entirely() {
        isolate_config();
        let st = state_two_jacks();
        st.enable_custom_client("codex").await.unwrap();
        st.set_client_override("codex", "alpha", false).await.unwrap();
        assert!(st.config.read().client_overrides.contains_key("codex"));

        st.delete_client("codex").await.unwrap();

        // Unlike disable_custom_client (which keeps the entry, just flipping
        // enabled to false), delete_client must REMOVE the entry entirely.
        assert!(
            !st.config.read().client_overrides.contains_key("codex"),
            "delete must erase the override entry, not just disable it"
        );
    }

    #[tokio::test]
    async fn delete_client_removes_forbidden_entry() {
        isolate_config();
        let st = state_two_jacks();
        st.record_seen_client("codex", None);
        st.set_forbidden("codex", true).await.unwrap();
        assert!(st.config.read().is_forbidden(Some("codex")));

        st.delete_client("codex").await.unwrap();

        let cfg = st.config.read();
        assert!(
            !cfg.is_forbidden(Some("codex")),
            "deleted identity gone from forbidden_clients"
        );
        assert!(
            !cfg.seen_clients.iter().any(|c| c.name == "codex"),
            "and gone from seen_clients too"
        );
    }

    #[tokio::test]
    async fn delete_client_removes_sole_custom_consumer_stops_jack() {
        isolate_config();
        let st = state_two_jacks(); // alpha ON, beta OFF globally
        st.record_seen_client("codex", None);
        // Enable Custom for codex and flip the globally-OFF beta ON for it, so
        // codex's override is the SOLE reason beta's shared child should run.
        st.enable_custom_client("codex").await.unwrap();
        st.set_client_override("codex", "beta", true).await.unwrap();
        assert!(
            st.config.read().should_run_jack("beta"),
            "codex's Custom override is the sole reason beta should run"
        );

        // Deleting codex removes that sole consumer.
        st.delete_client("codex").await.unwrap();

        assert!(
            !st.config.read().should_run_jack("beta"),
            "deleting the sole Custom consumer must stop the shared child"
        );
    }

    #[tokio::test]
    async fn delete_client_unknown_identity_is_noop() {
        isolate_config();
        let st = state_two_jacks();
        st.record_seen_client("codex", None);
        let seen_before = st.config.read().seen_clients.len();

        // An identity never seen / never customized / never forbidden.
        st.delete_client("ghost").await.unwrap();

        let cfg = st.config.read();
        assert_eq!(
            cfg.seen_clients.len(),
            seen_before,
            "unknown delete must not touch seen_clients"
        );
        assert!(
            !cfg.client_overrides.contains_key("ghost"),
            "unknown delete must not create an override entry"
        );
        assert!(
            !cfg.is_forbidden(Some("ghost")),
            "unknown delete must not touch forbidden_clients"
        );
    }

    // ---- Level-2 request-log toggle (save-then-commit) ----

    #[tokio::test]
    async fn set_request_logging_enabled_persists_then_commits() {
        isolate_config();
        let st = state_two_jacks();
        // Ships off (default).
        assert!(!st.config.read().request_logging_enabled);

        // Enable: save-then-commit means disk + memory both reflect true after.
        st.set_request_logging_enabled(true).unwrap();
        assert!(st.config.read().request_logging_enabled);
        let loaded = config::load_from_path(&config::config_file_path()).unwrap();
        assert!(
            loaded.request_logging_enabled,
            "disk must match in-memory state after a successful enable"
        );

        // Disable: both reflect false again.
        st.set_request_logging_enabled(false).unwrap();
        assert!(!st.config.read().request_logging_enabled);
        let loaded2 = config::load_from_path(&config::config_file_path()).unwrap();
        assert!(!loaded2.request_logging_enabled);

        // A no-op (already at the desired value) is Ok without a disk write.
        st.set_request_logging_enabled(false).unwrap();
        assert!(!st.config.read().request_logging_enabled);
    }

    // ---- (S13 §6.1) set_patched must SAVE THEN COMMIT --------------------

    /// Route `config::save` at a path that CANNOT be created, so every persist
    /// attempt fails deterministically.
    ///
    /// `save_to_path` calls `create_dir_all(parent)` first, so a merely missing
    /// directory is not enough — it would be created and the save would SUCCEED.
    /// Instead we create a real FILE and hang the config path underneath it:
    /// `create_dir_all` cannot make a directory inside a file, on any platform.
    fn isolate_config_unwritable() {
        let mut blocker = std::env::temp_dir();
        blocker.push(format!(
            "patchbay_blocker_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&blocker, b"not a directory").expect("create blocker file");
        let mut p = blocker.clone();
        p.push("subdir");
        p.push("patchbay.json");
        config::set_test_config_path(Some(p));
    }

    #[tokio::test]
    async fn set_patched_failed_save_does_not_commit_to_memory() {
        // Before S13 this method mutated the LIVE config first and only logged a
        // failed save, so runtime and disk silently disagreed on the app's most
        // frequent operation — and the window UI would have rendered the wrong
        // in-memory value with full confidence.
        isolate_config_unwritable();
        let st = state_with_prod();
        assert!(!st.config.read().jacks[0].patched, "template ships prod OFF");

        let result = st.set_patched("prod", true).await;

        assert!(
            !result.patched,
            "a failed persist must report the UNCHANGED flag, not the intent"
        );
        assert!(
            result.status.starts_with("failed to persist"),
            "status must name the failure, got '{}'",
            result.status
        );
        assert!(
            !st.config.read().jacks[0].patched,
            "live config must NOT be mutated when the save failed"
        );
    }

    #[tokio::test]
    async fn set_patched_commits_when_save_succeeds() {
        isolate_config();
        let st = state_with_prod();
        let result = st.set_patched("prod", true).await;
        assert!(result.patched);
        assert!(st.config.read().jacks[0].patched);
    }

    #[tokio::test]
    async fn set_patched_unknown_jack_is_reported_not_persisted() {
        isolate_config();
        let st = state_with_prod();
        let result = st.set_patched("no-such-jack", true).await;
        assert!(!result.patched);
        assert_eq!(result.status, "unknown");
    }

    // ---- (S13) touch_client_last_seen ------------------------------------

    #[test]
    fn touch_last_seen_updates_a_known_client() {
        isolate_config();
        let mut cfg = first_run_template();
        cfg.seen_clients.push(crate::config::SeenClient {
            name: "Codex".to_string(),
            first_seen_version: None,
            first_seen: "2026-07-29T16:26:20+02:00".to_string(),
            last_seen: None,
        });
        let st = AppState::new(cfg);

        st.touch_client_last_seen("Codex");

        let got = st.config.read().seen_clients[0].last_seen.clone();
        assert!(got.is_some(), "a known client's last_seen must be filled");
        assert_ne!(
            got.as_deref(),
            Some("2026-07-29T16:26:20+02:00"),
            "last_seen must be NOW, not a copy of first_seen"
        );
    }

    #[test]
    fn touch_last_seen_is_throttled_within_the_window() {
        isolate_config();
        let mut cfg = first_run_template();
        cfg.seen_clients.push(crate::config::SeenClient {
            name: "Codex".to_string(),
            first_seen_version: None,
            first_seen: "2026-07-29T16:26:20+02:00".to_string(),
            last_seen: None,
        });
        let st = AppState::new(cfg);

        st.touch_client_last_seen("Codex");
        let first = st.config.read().seen_clients[0].last_seen.clone();
        // A reconnect storm: many more calls, all inside the throttle window.
        for _ in 0..50 {
            st.touch_client_last_seen("Codex");
        }
        let after = st.config.read().seen_clients[0].last_seen.clone();

        assert_eq!(
            first, after,
            "within LAST_SEEN_THROTTLE_SECS the value must not be rewritten"
        );
        assert!(LAST_SEEN_THROTTLE_SECS >= 30, "throttle must be meaningful");
    }

    #[test]
    fn touch_last_seen_ignores_an_unknown_identity() {
        // Must never be able to resurrect a deleted or gate-denied identity:
        // recording a NEW client stays record_seen_client's job.
        isolate_config();
        let st = state_with_prod();
        let before = st.config.read().seen_clients.len();

        st.touch_client_last_seen("rogue-never-seen");

        assert_eq!(
            st.config.read().seen_clients.len(),
            before,
            "an unknown identity must not be added to seen_clients"
        );
    }

    #[test]
    fn record_seen_client_seeds_last_seen_on_first_sighting() {
        isolate_config();
        let st = state_with_prod();
        st.record_seen_client("brand-new", Some("1.0"));
        let cfg = st.config.read();
        let entry = cfg
            .seen_clients
            .iter()
            .find(|c| c.name == "brand-new")
            .expect("recorded");
        assert!(
            entry.last_seen.is_some(),
            "a first sighting IS a sighting and must seed last_seen"
        );
        assert_eq!(
            entry.last_seen.as_deref(),
            Some(entry.first_seen.as_str()),
            "on a first sighting both timestamps are the same moment"
        );
    }

    // ---- (S13 W2) the state bus reaches EVERY mutator ---------------------

    /// Every state-changing method must fan out to both interfaces.
    ///
    /// This is a SOURCE-level check, deliberately. The behavioural path runs
    /// through a Tauri `AppHandle` that unit tests do not have (`fan_out`
    /// returns early with no tray handle), so a runtime assertion here would
    /// pass vacuously and prove nothing. What actually needs guarding is the
    /// human failure the review caught: a mutator that quietly does not tell
    /// the UI. A future mutator added without a `notify_*` call fails this test
    /// with the method's own name in the message.
    #[test]
    fn every_mutator_fans_out_to_the_ui() {
        const SOURCE: &str = include_str!("app_state.rs");
        // Every method that can change what a user sees.
        // EVERY method that can change what a user sees. The first version of
        // this list omitted seven of them, which let the test report a complete
        // state bus while `touch_client_last_seen` and `set_ui_mode` notified
        // nobody. A list is only a proof of completeness if it is complete.
        const MUTATORS: &[&str] = &[
            "set_patched",
            "add_jack",
            "remove_jack",
            "set_client_override",
            "enable_custom_client",
            "disable_custom_client",
            "reset_custom_to_global",
            "set_forbidden",
            "set_forbidden_batch",
            "delete_client",
            "delete_agents",
            "undo_delete_agents",
            "set_request_logging_enabled",
            "set_autostart",
            "set_require_approval",
            "set_port",
            "set_ui_mode",
            "touch_client_last_seen",
            "record_seen_client",
            "apply_approval_decision",
        ];

        for name in MUTATORS {
            // Find the definition, then take everything up to the start of the
            // next item at the same indentation.
            let sig_async = format!("    pub async fn {}(", name);
            let sig_sync = format!("    pub fn {}(", name);
            let sig_private = format!("    fn {}(", name);
            let start = SOURCE
                .find(&sig_async)
                .or_else(|| SOURCE.find(&sig_sync))
                .or_else(|| SOURCE.find(&sig_private))
                .unwrap_or_else(|| panic!("mutator '{}' not found in app_state.rs", name));
            let rest = &SOURCE[start..];
            // The body ends at the first line that closes the method at method
            // indentation: a newline, four spaces, a closing brace, a newline.
            const METHOD_END: &str = "
    }
";
            let end = rest
                .find(METHOD_END)
                .map(|i| i + METHOD_END.len())
                .unwrap_or(rest.len());
            let body = &rest[..end];

            assert!(
                body.contains("notify_state_changed()")
                    || body.contains("notify_structure_changed()"),
                "mutator '{}' changes user-visible state but never fans out. Every mutator must call notify_state_changed() or notify_structure_changed() (WINDOW_UI_PLAN W-D4), or the tray and the window silently disagree.",
                name
            );
        }
    }

    #[test]
    fn set_ui_mode_persists_and_is_idempotent() {
        isolate_config();
        let st = state_with_prod();
        assert_eq!(st.config.read().ui_mode, crate::config::UiMode::Tray);

        st.set_ui_mode(crate::config::UiMode::Window).expect("save");
        assert_eq!(st.config.read().ui_mode, crate::config::UiMode::Window);

        // A no-op set must not rewrite the file (the early return), and must
        // still report success.
        st.set_ui_mode(crate::config::UiMode::Window).expect("no-op");
        assert_eq!(st.config.read().ui_mode, crate::config::UiMode::Window);
    }

    // ---- (S13 W-D11) client-name sanitization ----------------------------

    #[test]
    fn sanitize_keeps_every_real_agent_name_untouched() {
        // Regression guard with the actual identities from this machine: if
        // sanitizing renamed them, every existing per-agent override and denial
        // would silently stop matching.
        for name in [
            "Claude Code - Personal",
            "Claude Code - Work",
            "Claude Code - MiniMax expert",
            "Antigravity-CLI",
            "Kilo-Agent",
            "OpenCode",
            "opencode",
            "Codex",
            "bee-memory-bank",
            "agent@host",
            "tool/sub",
            "v1.2.3+build",
        ] {
            assert_eq!(sanitize_client_name(name), name, "must not rewrite '{}'", name);
        }
    }

    #[test]
    fn sanitize_defuses_markup_in_an_agent_name() {
        // The whole point of W-D11 layer 1: this string is chosen by whoever
        // connects, and the window renders it.
        let hostile = "<img src=x onerror=\"alert(1)\">";
        let safe = sanitize_client_name(hostile);
        for ch in ['<', '>', '"', '=', '(', ')'] {
            assert!(!safe.contains(ch), "'{}' survived in {:?}", ch, safe);
        }
    }

    #[test]
    fn sanitize_replaces_rather_than_deletes_so_tampering_stays_visible() {
        // Deleting the disallowed characters would turn a hostile name into an
        // innocent-looking one; replacing them leaves the scar visible.
        let safe = sanitize_client_name("<script>");
        assert_eq!(safe, "_script_");
        assert_ne!(safe, "script", "a sanitized name must not masquerade as a plain one");
    }

    #[test]
    fn sanitize_caps_length_and_never_returns_empty() {
        let long = "x".repeat(500);
        assert_eq!(
            sanitize_client_name(&long).chars().count(),
            MAX_CLIENT_NAME_LEN
        );
        assert_eq!(sanitize_client_name("   "), "unnamed-agent");
        assert_eq!(sanitize_client_name("\u{0}\u{1}"), "unnamed-agent");
    }

    #[test]
    fn sanitize_strips_newlines_that_would_forge_log_lines() {
        // The identity is written into the diagnostic log; an embedded newline
        // would let an agent inject a fake log entry.
        let forged = "ok\n[EVENT] custom_disable 'Claude Code - Work'";
        let safe = sanitize_client_name(forged);
        assert!(!safe.contains('\n'));
    }

    // ---- (S13 W-D12) entity-scoped undo ----------------------------------

    fn state_with_agents() -> AppState {
        let mut cfg = first_run_template();
        for name in ["keep-me", "junk-a", "junk-b"] {
            cfg.seen_clients.push(crate::config::SeenClient {
                name: name.to_string(),
                first_seen_version: Some("1.0".to_string()),
                first_seen: "2026-08-18T15:00:00+02:00".to_string(),
                last_seen: None,
            });
        }
        cfg.client_overrides.insert(
            "junk-a".to_string(),
            crate::config::ClientOverride {
                enabled: true,
                jacks: BTreeMap::from([("prod".to_string(), true)]),
            },
        );
        AppState::new(cfg)
    }

    #[tokio::test]
    async fn undo_restores_only_the_deleted_agents() {
        isolate_config();
        let st = state_with_agents();

        let token = st
            .delete_agents(&["junk-a".to_string(), "junk-b".to_string()])
            .await;
        {
            let cfg = st.config.read();
            assert!(!cfg.seen_clients.iter().any(|c| c.name == "junk-a"));
            assert!(!cfg.seen_clients.iter().any(|c| c.name == "junk-b"));
            assert!(cfg.seen_clients.iter().any(|c| c.name == "keep-me"));
            assert!(!cfg.client_overrides.contains_key("junk-a"));
        }

        let restored = st.undo_delete_agents(token).await.expect("undo");
        assert_eq!(restored, 2);
        let cfg = st.config.read();
        assert!(cfg.seen_clients.iter().any(|c| c.name == "junk-a"));
        assert!(cfg.seen_clients.iter().any(|c| c.name == "junk-b"));
        assert!(
            cfg.client_overrides.contains_key("junk-a"),
            "the agent's Custom list must come back with it"
        );
    }

    #[tokio::test]
    async fn undo_does_not_clobber_a_concurrent_unrelated_change() {
        // THE bug the review found in plan v1: restoring a whole-config
        // snapshot would silently revert everything else that happened during
        // the undo window. Here a jack is toggled and a new agent appears
        // between the delete and the undo; both must survive.
        isolate_config();
        let st = state_with_agents();

        let token = st.delete_agents(&["junk-a".to_string()]).await;

        st.set_patched("prod", true).await;
        st.record_seen_client("arrived-meanwhile", Some("2.0"));

        st.undo_delete_agents(token).await.expect("undo");

        let cfg = st.config.read();
        assert!(
            cfg.seen_clients.iter().any(|c| c.name == "junk-a"),
            "the deleted agent must be restored"
        );
        assert!(
            cfg.jacks.iter().any(|j| j.name == "prod" && j.patched),
            "a jack toggled during the undo window must NOT be reverted"
        );
        assert!(
            cfg.seen_clients.iter().any(|c| c.name == "arrived-meanwhile"),
            "an agent registered during the undo window must NOT be erased"
        );
    }

    #[tokio::test]
    async fn a_stale_undo_token_is_refused() {
        isolate_config();
        let st = state_with_agents();

        let first = st.delete_agents(&["junk-a".to_string()]).await;
        let second = st.delete_agents(&["junk-b".to_string()]).await;
        assert_ne!(first, second);

        // Clicking the old strip must not undo the NEW deletion.
        let err = st.undo_delete_agents(first).await.unwrap_err();
        assert!(err.contains("no longer available"), "got '{}'", err);

        st.undo_delete_agents(second).await.expect("the current undo still works");
        assert!(st.config.read().seen_clients.iter().any(|c| c.name == "junk-b"));
        assert!(
            !st.config.read().seen_clients.iter().any(|c| c.name == "junk-a"),
            "the superseded deletion stays applied"
        );
    }

    #[tokio::test]
    async fn undo_is_single_use() {
        isolate_config();
        let st = state_with_agents();
        let token = st.delete_agents(&["junk-a".to_string()]).await;
        st.undo_delete_agents(token).await.expect("first undo");
        assert!(st.undo_delete_agents(token).await.is_err(), "must not undo twice");
    }

    #[tokio::test]
    async fn deleting_a_denied_identity_restores_the_denial() {
        // A denied identity has no seen_clients row at all — only a
        // forbidden_clients entry. Undo must bring the DENIAL back, or undoing
        // would silently re-admit an agent the user had blocked.
        isolate_config();
        let mut cfg = first_run_template();
        cfg.forbidden_clients.push("rogue".to_string());
        let st = AppState::new(cfg);

        let token = st.delete_agents(&["rogue".to_string()]).await;
        assert!(st.config.read().forbidden_clients.is_empty());

        st.undo_delete_agents(token).await.expect("undo");
        assert!(
            st.config.read().forbidden_clients.iter().any(|f| f == "rogue"),
            "undo must not silently un-block a blocked agent"
        );
    }

    #[tokio::test]
    async fn reset_custom_to_global_drops_the_preserved_list() {
        // The operation the tray cannot express: after this, enabling Custom
        // seeds fresh from the global list instead of restoring the old map.
        isolate_config();
        let st = state_with_agents();
        assert!(st.config.read().client_overrides.contains_key("junk-a"));

        st.reset_custom_to_global("junk-a").await.expect("reset");
        assert!(
            !st.config.read().client_overrides.contains_key("junk-a"),
            "the override entry must be gone, not merely disabled"
        );

        // And a second call is a harmless no-op.
        st.reset_custom_to_global("junk-a").await.expect("no-op");
    }
}
