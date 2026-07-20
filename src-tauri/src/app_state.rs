//! Process-wide shared application state (MASTER_PLAN module tree:
//! `app_state.rs`).
//!
//! Cloned cheaply (all fields are `Arc`) and passed to axum handlers as
//! `State<AppState>`. Holds the live config, the client-session registry, and
//! the gateway lifecycle status.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

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
            pending_approvals: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Inject the Tauri app handle once the tray exists (S10). Called from
    /// `main.rs` setup shortly after the tray is built. Subsequent calls (e.g.
    /// after a retry-gateway rebind that reuses the same state) are harmless.
    pub fn set_tray_handle(&self, handle: AppHandle) {
        *self.tray_handle.write() = Some(handle);
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
        let jack_config = {
            let mut cfg = self.config.write();
            let jack = match cfg.jacks.iter_mut().find(|j| j.name == jack_name) {
                Some(j) => j,
                None => {
                    log(&format!("set_patched: unknown jack '{}'", jack_name));
                    return ToggleResult {
                        patched: false,
                        status: "unknown".to_string(),
                    };
                }
            };
            jack.patched = patched;
            let snapshot = cfg.clone();
            drop(cfg);
            if let Err(e) = config::save(&snapshot) {
                log(&format!("set_patched: failed to persist config: {}", e));
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
        // config::save() can never leave memory and disk diverged.
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
            if let Err(e) = config::save(&snapshot) {
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
            if let Err(e) = config::save(&snapshot) {
                log(&format!("remove_jack: failed to persist config: {}", e));
                return Err(RemoveJackError::PersistFailed(e));
            }
            *self.config.write() = snapshot;
        }

        // 5. Broadcast so connected clients drop the jack's tools.
        self.sessions.broadcast_tools_list_changed().await;
        crate::utils::request_log::log_event(self, &format!("remove_jack '{}'", name));
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
            let mut cfg = self.config.write();
            if cfg.seen_clients.iter().any(|c| c.name == name) {
                return;
            }
            cfg.seen_clients.push(crate::config::SeenClient {
                name: name.to_string(),
                first_seen_version: version.map(str::to_owned),
                first_seen: chrono::Local::now().to_rfc3339(),
            });
            let snap = cfg.clone();
            drop(cfg);
            if let Err(e) = config::save(&snap) {
                log(&format!("record_seen_client: failed to persist: {}", e));
            }
        }
        {
            log(&format!("record_seen_client: new client '{}' recorded", name));
            // Rebuild the menu so the agent appears in the "Custom" submenu. This
            // crosses from a gateway worker (no AppHandle) onto the main thread;
            // reuse the same spawn + rebuild_menu pattern the tray handlers use.
            if let Some(h) = self.tray_handle.read().clone() {
                tauri::async_runtime::spawn(async move {
                    crate::tray::rebuild_menu_and_refresh(&h);
                });
            }
        }
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
            if let Err(e) = config::save(&snap) {
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
            if let Err(e) = config::save(&snap) {
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
                std::thread::spawn(move || {
                    let allowed = crate::approval::show_approval_dialog(&name_owned);
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
        //    config::save() can never leave memory and disk diverged. On
        //    Allow: record seen_clients (with version) + clear stale
        //    forbidden. On Deny: add to forbidden.
        let (candidate, changed) = {
            let cfg = self.config.read();
            let mut candidate = cfg.clone();
            let changed = if allowed {
                let mut changed = false;
                if !candidate.seen_clients.iter().any(|c| c.name == name) {
                    candidate.seen_clients.push(crate::config::SeenClient {
                        name: name.to_string(),
                        first_seen_version: version.map(str::to_owned),
                        first_seen: chrono::Local::now().to_rfc3339(),
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
            if let Err(e) = config::save(&candidate) {
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
            if let Some(h) = self.tray_handle.read().clone() {
                tauri::async_runtime::spawn(async move {
                    crate::tray::rebuild_menu_and_refresh(&h);
                });
            }

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
        if let Err(e) = config::save(&candidate) {
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
        if let Err(e) = config::save(&candidate) {
            log(&format!("disable_custom_client: failed to persist: {}", e));
            return Err(e);
        }
        *self.config.write() = candidate;
        self.reconcile_all_jack_lifecycles().await;
        crate::utils::request_log::log_event(
            self,
            &format!("custom_disable '{}'", client_name),
        );
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
        if let Err(e) = config::save(&candidate) {
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
        config::save(&candidate)?;
        *self.config.write() = candidate;
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
#[derive(Clone, Debug)]
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

    fn state_two_jacks() -> AppState {
        // `alpha` patched ON, `beta` patched OFF globally.
        let cfg = crate::config::PatchbayConfig {
            version: crate::config::CURRENT_VERSION,
            port: crate::config::DEFAULT_PORT,
            autostart: false,
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
}
