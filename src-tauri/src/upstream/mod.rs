//! `UpstreamManager`: one shared upstream connection per patched jack, a merged
//! tool cache, and call routing (MASTER_PLAN S4 / D4; S6 adds streamable-http).
//!
//! Holds the process-wide Job Object + a `RwLock<HashMap<jack_name, JackRuntime>>`.
//! Each `JackRuntime` carries a lifecycle `status`, the connected upstream client
//! (a `dyn UpstreamClient` — stdio or http — when Running), and a `tool_cache`
//! snapshot for the gateway merge.
//!
//! A per-jack supervisor task (spawned on a successful start) drains the
//! client's event receiver: `ToolsListChanged` -> refresh the cache + broadcast;
//! `Exited` -> mark Failed, clear the cache, broadcast. Full exponential
//! backoff/retry is deferred beyond v0.1's minimum.

pub mod client;
pub mod http;
pub mod process;
pub mod stdio;

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::sync::Mutex as TokioMutex;

use crate::config::secrets;
use crate::config::{JackConfig, JackTransport, PatchbayConfig};
use crate::gateway::session::SessionRegistry;
use crate::upstream::client::{ClientEvent, UpstreamClient};
use crate::upstream::http::HttpClient;
use crate::upstream::process::Job;
use crate::upstream::stdio::StdioClient;
use crate::utils::log::log;

/// Per-jack lifecycle status (surfaced to the tray tooltip in S5).
#[derive(Clone, Debug)]
pub enum JackStatus {
    /// Not started / explicitly stopped.
    Stopped,
    /// Spawn + handshake in progress.
    Starting,
    /// Child running, handshake done, tools cached.
    Running,
    /// Start or runtime failed; the string is the last error.
    Failed(String),
}

impl JackStatus {
    fn is_running(&self) -> bool {
        matches!(self, JackStatus::Running)
    }
}

/// Runtime state for one jack.
pub struct JackRuntime {
    pub status: JackStatus,
    pub client: Option<Arc<dyn UpstreamClient>>,
    pub tool_cache: Vec<Value>,
}

/// Manages all upstream MCP children + the merged tool view.
pub struct UpstreamManager {
    /// Process-wide Job Object (KILL_ON_JOB_CLOSE) — the zero-orphan backstop.
    job: Option<Arc<Job>>,
    /// Per-jack runtime state. Keys are jack names.
    runtimes: Arc<RwLock<HashMap<String, JackRuntime>>>,
    /// Per-jack async locks serializing the toggle pipeline (start/stop) so a
    /// fast double-toggle or a toggle racing reload can't spawn two children.
    toggle_locks: Mutex<HashMap<String, Arc<TokioMutex<()>>>>,
}

impl Default for UpstreamManager {
    fn default() -> Self {
        Self::new()
    }
}

impl UpstreamManager {
    /// Build the manager and create the process-wide Job Object. Safe to call
    /// off the async runtime (the syscall is synchronous); never panics — a Job
    /// creation failure is logged and the manager simply runs without
    /// orphan-kill protection.
    pub fn new() -> Self {
        let job = Job::create_kill_on_close().map(Arc::new);
        UpstreamManager {
            job,
            runtimes: Arc::new(RwLock::new(HashMap::new())),
            toggle_locks: Mutex::new(HashMap::new()),
        }
    }

    /// Acquire the per-jack async toggle lock, returning an owned guard. The
    /// whole toggle pipeline (flip + start/stop) holds this so concurrent
    /// toggles / reloads on the same jack serialize (one child at a time).
    pub async fn jack_lock(&self, name: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self.toggle_locks.lock();
            locks
                .entry(name.to_string())
                .or_insert_with(|| Arc::new(TokioMutex::new(())))
                .clone()
        };
        lock.lock_owned().await
    }

    /// Snapshot of all Running jacks' tools: `(jack_name, upstream_tool_json)`.
    /// The gateway `tools/list` merge namespaces each entry. Only Running jacks
    /// contribute (Starting/Failed/Stopped expose nothing).
    pub fn cached_tools(&self) -> Vec<(String, Value)> {
        let runtimes = self.runtimes.read();
        let mut out = Vec::new();
        for (name, rt) in runtimes.iter() {
            // (FIX 4) belt-and-suspenders: an invalidly-named jack never
            // contributes to the tools/list merge even if it somehow ran.
            if rt.status.is_running() && crate::config::is_valid_jack_name(name) {
                for tool in &rt.tool_cache {
                    out.push((name.clone(), tool.clone()));
                }
            }
        }
        out
    }

    /// Start (connect + initialize + list_tools) one jack — stdio or
    /// streamable-http. On any failure records `Failed(reason)` and logs; never
    /// panics.
    pub async fn start_jack(
        &self,
        jack: &JackConfig,
        sessions: Arc<SessionRegistry>,
        config: Arc<RwLock<PatchbayConfig>>,
    ) {
        // (FIX 4) Skip jacks whose name is invalid (bad charset / contains __ /
        // empty / too long): they would collide or break the `<jack>__<tool>`
        // namespace split. They never reach Running, so they stay out of the
        // tools/list merge too.
        if !crate::config::is_valid_jack_name(&jack.name) {
            log(&format!(
                "upstream: skipping invalidly-named jack '{}' (not started)",
                jack.name
            ));
            return;
        }
        // (b) If a client for this name is already live, stop it FIRST so we
        // never spawn a second child (which would otherwise leak).
        {
            let has_client = self
                .runtimes
                .read()
                .get(&jack.name)
                .map(|r| r.client.is_some())
                .unwrap_or(false);
            if has_client {
                log(&format!(
                    "upstream: stopping old client for '{}' before re-start",
                    jack.name
                ));
                self.stop_jack(&jack.name).await;
            }
        }
        // Mark Starting up-front so a concurrent cached_tools() sees intent.
        self.set_status(&jack.name, JackStatus::Starting, None);
        log(&format!("upstream: starting jack '{}'", jack.name));

        // Build the per-transport client + supervisor event stream. Both arms
        // produce an `Arc<dyn UpstreamClient>`; the rest of the pipeline
        // (handshake -> cache -> supervise) is identical.
        let (client, events): (
            Arc<dyn UpstreamClient>,
            mpsc::UnboundedReceiver<ClientEvent>,
        ) = match &jack.transport {
            JackTransport::Stdio { command, args, .. } => {
                if command.is_empty() {
                    self.set_failed(&jack.name, "stdio jack has an empty command".to_string());
                    return;
                }
                // Decrypt secrets at spawn only (never hold plaintext long-term).
                let env = secrets::decrypted_env(jack);
                match StdioClient::spawn(
                    jack.name.clone(),
                    command.clone(),
                    args.clone(),
                    env,
                    self.job.as_deref(),
                )
                .await
                {
                    Ok((c, ev)) => {
                        // Coerce `Arc<StdioClient>` -> `Arc<dyn UpstreamClient>`
                        // at a guaranteed coercion site (let binding type ascription).
                        let c: Arc<dyn UpstreamClient> = c;
                        (c, ev)
                    }
                    Err(e) => {
                        self.set_failed(&jack.name, format!("spawn failed: {}", e));
                        return;
                    }
                }
            }
            JackTransport::StreamableHttp { url, .. } => {
                if url.is_empty() {
                    self.set_failed(
                        &jack.name,
                        "streamable-http jack has an empty url".to_string(),
                    );
                    return;
                }
                // Decrypt headers at connect only (held for the connection
                // lifetime; never persisted in plaintext).
                let headers = secrets::decrypted_headers(jack);
                let (c, ev) = HttpClient::connect(jack.name.clone(), url.clone(), headers);
                let c: Arc<dyn UpstreamClient> = c;
                (c, ev)
            }
        };

        // MCP handshake.
        if let Err(e) = client.initialize().await {
            self.set_failed(&jack.name, format!("initialize failed: {}", e));
            client.shutdown().await;
            return;
        }

        // Seed the tool cache.
        let tools = match client.list_tools().await {
            Ok(t) => t,
            Err(e) => {
                self.set_failed(&jack.name, format!("tools/list failed: {}", e));
                client.shutdown().await;
                return;
            }
        };

        let n = tools.len();

        // (b) Re-check whether the child is still wanted: a concurrent
        // toggle/reload may have flipped this jack off globally AND no Custom
        // client needs it while we were initializing. If so, tear the fresh
        // client down and leave it Stopped instead of Running. (S10) This goes
        // through `should_run_jack` — the GLOBAL OR any-enabled-Custom-client
        // aggregate — so a child a Custom client relies on stays up.
        let still_needed = config.read().should_run_jack(&jack.name);
        if !still_needed {
            log(&format!(
                "upstream: jack '{}' no longer needed after start; aborting",
                jack.name
            ));
            client.shutdown().await;
            self.set_status(&jack.name, JackStatus::Stopped, None);
            return;
        }

        self.set_status(
            &jack.name,
            JackStatus::Running,
            Some((client.clone(), tools)),
        );
        log(&format!("upstream: jack '{}' running with {} tools", jack.name, n));

        // Supervisor: refresh on upstream list_changed, fail on unexpected exit.
        self.spawn_supervisor(jack.name.clone(), client, events, sessions);
    }

    /// Spawn the per-jack supervisor task draining `events`.
    fn spawn_supervisor(
        &self,
        name: String,
        client: Arc<dyn UpstreamClient>,
        mut events: mpsc::UnboundedReceiver<ClientEvent>,
        sessions: Arc<SessionRegistry>,
    ) {
        let runtimes = self.runtimes.clone();
        tokio::spawn(async move {
            while let Some(ev) = events.recv().await {
                match ev {
                    ClientEvent::ToolsListChanged => {
                        log(&format!(
                            "upstream: jack '{}' sent tools/list_changed",
                            name
                        ));
                        // (c) Ignore stale supervisors: only act if the runtime's
                        // CURRENT client is still THIS supervisor's Arc.
                        let owns_current = runtimes
                            .read()
                            .get(&name)
                            .map(|r| {
                                r.status.is_running()
                                    && r
                                        .client
                                        .as_ref()
                                        .map(|c| Arc::ptr_eq(c, &client))
                                        .unwrap_or(false)
                            })
                            .unwrap_or(false);
                        if !owns_current {
                            continue;
                        }
                        match client.list_tools().await {
                            Ok(new_tools) => {
                                // Re-check identity + running AFTER the await too.
                                let still_owns = runtimes
                                    .read()
                                    .get(&name)
                                    .map(|r| {
                                        r.status.is_running()
                                            && r
                                                .client
                                                .as_ref()
                                                .map(|c| Arc::ptr_eq(c, &client))
                                                .unwrap_or(false)
                                    })
                                    .unwrap_or(false);
                                if still_owns {
                                    {
                                        let mut rt = runtimes.write();
                                        if let Some(r) = rt.get_mut(&name) {
                                            r.tool_cache = new_tools;
                                        }
                                    }
                                    sessions.broadcast_tools_list_changed().await;
                                }
                            }
                            Err(e) => {
                                log(&format!(
                                    "upstream: jack '{}' re-list after list_changed failed: {}",
                                    name, e
                                ));
                            }
                        }
                    }
                    ClientEvent::Exited(reason) => {
                        log(&format!("upstream: jack '{}' exited: {}", name, reason));
                        // (c) Only a supervisor whose client is still current may
                        // mark Failed — a stale supervisor must not touch the new
                        // runtime (a re-started jack stays Running).
                        let owns_current = runtimes
                            .read()
                            .get(&name)
                            .map(|r| {
                                r.client
                                    .as_ref()
                                    .map(|c| Arc::ptr_eq(c, &client))
                                    .unwrap_or(false)
                            })
                            .unwrap_or(false);
                        if owns_current {
                            let was_running = runtimes
                                .read()
                                .get(&name)
                                .map(|r| r.status.is_running())
                                .unwrap_or(false);
                            if was_running {
                                {
                                    let mut rt = runtimes.write();
                                    if let Some(r) = rt.get_mut(&name) {
                                        r.status = JackStatus::Failed(reason.clone());
                                        r.client = None;
                                        r.tool_cache.clear();
                                    }
                                }
                                sessions.broadcast_tools_list_changed().await;
                            }
                        }
                        break;
                    }
                }
            }
            // Drop this task's client clone last so the child is reaped once no
            // handler/supervisor references it (the runtime map already cleared
            // its slot on exit).
            drop(client);
        });
    }

    /// Stop one jack: shutdown the client, clear its cache, mark Stopped.
    pub async fn stop_jack(&self, name: &str) {
        let client = {
            let mut rt = self.runtimes.write();
            let client = rt.get_mut(name).and_then(|r| r.client.take());
            if let Some(r) = rt.get_mut(name) {
                r.status = JackStatus::Stopped;
                r.tool_cache.clear();
            }
            client
        };
        if let Some(client) = client {
            client.shutdown().await;
            log(&format!("upstream: jack '{}' stopped", name));
        } else {
            // Ensure the runtime exists in a Stopped state even if never started.
            self.set_status(name, JackStatus::Stopped, None);
        }
    }

    /// Route a `tools/call` to a Running jack's child. Returns `Err(reason)` if
    /// the jack is unknown or not Running (the polished D2 taxonomy is S5; here
    /// the gateway maps the error to a simple JSON-RPC error).
    pub async fn route_call(
        &self,
        jack_name: &str,
        tool_name: &str,
        arguments: Value,
    ) -> Result<Value, String> {
        let client = {
            let rt = self.runtimes.read();
            match rt.get(jack_name) {
                Some(r) if r.status.is_running() => r.client.clone(),
                Some(r) => {
                    return Err(format!(
                        "jack '{}' is not running (status: {:?})",
                        jack_name, r.status
                    ))
                }
                None => return Err(format!("unknown jack '{}'", jack_name)),
            }
        };
        let client =
            client.ok_or_else(|| format!("jack '{}' has no active client", jack_name))?;
        client.call_tool(tool_name, arguments).await
    }

    /// Is the named jack currently `Running`? Used by the gateway `tools/call`
    /// D2 taxonomy (case 3: patched but not Running) and the tray tooltip.
    pub fn is_jack_running(&self, name: &str) -> bool {
        self.runtimes
            .read()
            .get(name)
            .map(|r| r.status.is_running())
            .unwrap_or(false)
    }

    /// One-word status string for UI/tooltip/test reporting:
    /// `running` / `starting` / `stopped` / `failed: <reason>` / `unknown`.
    /// The gateway `tools/call` taxonomy maps this onto the "last error" phrase.
    pub fn status_string(&self, name: &str) -> String {
        let runtimes = self.runtimes.read();
        match runtimes.get(name) {
            Some(rt) => match &rt.status {
                JackStatus::Running => "running".to_string(),
                JackStatus::Starting => "starting".to_string(),
                JackStatus::Stopped => "stopped".to_string(),
                JackStatus::Failed(reason) => format!("failed: {}", reason),
            },
            None => "unknown".to_string(),
        }
    }

    // ---- internal helpers (all hold their write lock only briefly) ----------

    /// Force a jack into Failed(reason), clearing its client + cache.
    fn set_failed(&self, name: &str, reason: String) {
        log(&format!("upstream: jack '{}' FAILED: {}", name, reason));
        self.runtimes.write().insert(
            name.to_string(),
            JackRuntime {
                status: JackStatus::Failed(reason),
                client: None,
                tool_cache: Vec::new(),
            },
        );
    }

    /// Set a jack's status, optionally installing a client + tool cache (only
    /// meaningful for Running). Inserts/overwrites the runtime entry.
    fn set_status(
        &self,
        name: &str,
        status: JackStatus,
        running: Option<(Arc<dyn UpstreamClient>, Vec<Value>)>,
    ) {
        let mut rt = self.runtimes.write();
        match running {
            Some((client, tools)) => {
                rt.insert(
                    name.to_string(),
                    JackRuntime {
                        status,
                        client: Some(client),
                        tool_cache: tools,
                    },
                );
            }
            None => {
                rt.entry(name.to_string())
                    .and_modify(|r| {
                        r.status = status.clone();
                    })
                    .or_insert_with(|| JackRuntime {
                        status: status.clone(),
                        client: None,
                        tool_cache: Vec::new(),
                    });
            }
        }
    }
}
