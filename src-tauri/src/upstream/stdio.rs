//! A connected stdio MCP client: one shared child per jack (MASTER_PLAN D4).
//!
//! Framing is newline-delimited JSON-RPC over the child's stdin/stdout:
//! - A single reader task drains stdout line-by-line. A line carrying an `id`
//!   resolves the matching pending request (`oneshot`); a notification
//!   (`method`, no `id`) of `notifications/tools/list_changed` is surfaced as a
//!   [`ClientEvent`] so the manager can invalidate the cache + rebroadcast.
//! - Outbound requests use an upstream-local monotonic `id`, register a oneshot,
//!   write `<json>\n`, and await with a timeout.
//! - stderr is piped to a task that logs each line as `[jack:<name>] <line>`.
//!
//! The child is assigned to the process-wide Job Object right after spawn, so a
//! Patchbay crash/kill can never leave an orphan behind.

use std::collections::{BTreeMap, HashMap};
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{mpsc, Mutex as TokioMutex};
use tokio::task::JoinHandle;

use crate::upstream::client::{ClientEvent, UpstreamClient};
use crate::upstream::process::Job;
use crate::utils::log::log;

/// MCP protocol version we offer upstreams (MASTER_PLAN D4).
const PROTOCOL_VERSION: &str = "2025-06-18";

/// Handshake (`initialize`) timeout.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// Per-call (`tools/list`, `tools/call`) timeout.
const CALL_TIMEOUT: Duration = Duration::from_secs(120);
/// Upper bound on a single stdout line before we declare the upstream broken
/// (FIX 13): an unterminated line must not grow memory unbounded.
const MAX_LINE_BYTES: usize = 10 * 1024 * 1024;

/// `CREATE_NO_WINDOW` — applied so a stdio child never flashes a console window.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// A pending upstream request awaiting its JSON-RPC response. The channel
/// carries `Err` when the child dies before responding (or the request times
/// out via the per-id removal path), and `Ok(full_response_value)` otherwise.
type PendingTx = mpsc::Sender<Result<Value, String>>;

/// One connected stdio MCP child. Cheaply shared behind an `Arc`; the reader and
/// stderr tasks hold their own clones of the shared state.
pub struct StdioClient {
    /// Jack name, used purely for log prefixes.
    name: String,
    /// Stdin writer, guarded so outbound writes are serialized. `None` once the
    /// child is shut down (subsequent writes fail fast).
    stdin: Arc<TokioMutex<Option<ChildStdin>>>,
    /// Pending request map: upstream-local `id` -> response channel.
    pending: Arc<parking_lot::Mutex<HashMap<i64, PendingTx>>>,
    /// Monotonic source of fresh upstream-local request ids.
    next_id: AtomicI64,
    /// The child itself, taken + killed on shutdown.
    child: Arc<TokioMutex<Option<Child>>>,
    /// Signals to the manager's supervisor (list_changed / exit).
    event_tx: mpsc::UnboundedSender<ClientEvent>,
    /// Handles to the background tasks, aborted on shutdown.
    tasks: TokioMutex<Vec<JoinHandle<()>>>,
}

impl StdioClient {
    /// Spawn the child, assign it to the Job Object, and start the reader/stderr
    /// tasks. Returns the shared client and the supervisor event receiver.
    ///
    /// `env` must already be **decrypted** (caller runs
    /// `config::secrets::decrypted_env` at spawn, per MASTER_PLAN D4 — secrets
    /// are never held in plaintext beyond the spawn site).
    pub async fn spawn(
        name: String,
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        job: Option<&Job>,
    ) -> Result<(Arc<Self>, mpsc::UnboundedReceiver<ClientEvent>), String> {
        // On Windows, npx/npm/yarn/pnpm are `.cmd` launchers that `Command::new`
        // cannot resolve directly (it only tries `<name>` and `<name>.exe`, not
        // PATHEXT). Route a BARE command (no path separator, not `.exe`) through
        // `cmd /C` so `.cmd`/`.bat` resolve — this is what real MCP clients do on
        // Windows. A pathed / `.exe` command is spawned directly. The Job Object
        // kills the whole tree (cmd -> npx -> node), so no orphan escapes.
        #[cfg(windows)]
        let mut cmd = {
            let bare = !command.contains('/')
                && !command.contains('\\')
                && !command.to_ascii_lowercase().ends_with(".exe");
            if bare {
                let mut c = tokio::process::Command::new("cmd");
                c.arg("/C").arg(&command).args(&args);
                c
            } else {
                let mut c = tokio::process::Command::new(&command);
                c.args(&args);
                c
            }
        };
        #[cfg(not(windows))]
        let mut cmd = {
            let mut c = tokio::process::Command::new(&command);
            c.args(&args);
            c
        };
        // env = process env (inherited) + decrypted jack env overrides.
        cmd.envs(&env);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        // (d) Belt-and-braces: if the Child is dropped without an explicit
        // shutdown, kill the whole process so it can't leak as an orphan.
        cmd.kill_on_drop(true);
        // Never flash a console window for the child.
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return Err(format!(
                    "spawn '{} {}': {}",
                    command,
                    args.join(" "),
                    e
                ))
            }
        };

        // Assign to the process-wide Job Object immediately (by pid), so even a
        // hard crash can't orphan the child.
        if let Some(job) = job {
            if let Some(pid) = child.id() {
                job.assign_pid(pid);
            }
        }

        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        let pending = Arc::new(parking_lot::Mutex::new(
            HashMap::<i64, PendingTx>::new(),
        ));
        let stdin = Arc::new(TokioMutex::new(Some(stdin)));
        let child = Arc::new(TokioMutex::new(Some(child)));
        let (event_tx, event_rx) = mpsc::unbounded_channel::<ClientEvent>();

        let client = Arc::new(StdioClient {
            name: name.clone(),
            stdin: stdin.clone(),
            pending: pending.clone(),
            next_id: AtomicI64::new(1),
            child: child.clone(),
            event_tx: event_tx.clone(),
            tasks: TokioMutex::new(Vec::new()),
        });

        // ---- stderr -> log task ----
        let stderr_name = name.clone();
        let stderr_task = tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => break, // EOF
                    Ok(_) => {
                        let trimmed = line.trim_end_matches(['\r', '\n']);
                        if !trimmed.is_empty() {
                            log(&format!("[jack:{}] {}", stderr_name, trimmed));
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        // ---- stdout reader task: id correlation + notification surfacing ----
        let reader_name = name.clone();
        let reader_tx = event_tx.clone();
        // A clone of the stdin handle so the reader can reply to server-initiated
        // requests (ping/roots/list/...) without disturbing the pending map.
        let reader_stdin = stdin.clone();
        let reader_task = tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => {
                        // EOF: child's stdout closed -> it has exited.
                        log(&format!(
                            "[jack:{}] stdout EOF (upstream exited)",
                            reader_name
                        ));
                        drain_pending(&pending, "upstream exited");
                        let _ = reader_tx.send(ClientEvent::Exited("stdout closed".to_string()));
                        break;
                    }
                    Ok(_) => {
                        // (FIX 13) Cap an unterminated line so memory can't grow
                        // unbounded; fail the jack instead.
                        if line.len() > MAX_LINE_BYTES {
                            log(&format!(
                                "[jack:{}] stdout line exceeded {} bytes; failing jack",
                                reader_name, MAX_LINE_BYTES
                            ));
                            drain_pending(&pending, "line too long");
                            let _ = reader_tx
                                .send(ClientEvent::Exited("line too long".to_string()));
                            break;
                        }
                        let trimmed = line.trim_end_matches(['\r', '\n']);
                        if trimmed.is_empty() {
                            continue;
                        }
                        match serde_json::from_str::<Value>(trimmed) {
                            Ok(v) => {
                                let is_response = v.get("result").is_some()
                                    || v.get("error").is_some();
                                let has_method = v.get("method").is_some();
                                let has_id = v
                                    .get("id")
                                    .map(|i| !i.is_null())
                                    .unwrap_or(false);

                                if is_response && !has_method {
                                    // A RESPONSE (has result/error, no method):
                                    // resolve the pending request by id. Only a
                                    // numeric id can match our monotonic counter.
                                    if let Some(id) = v.get("id").and_then(|i| i.as_i64()) {
                                        let tx = pending.lock().remove(&id);
                                        if let Some(tx) = tx {
                                            let _ = tx.try_send(Ok(v));
                                        }
                                    }
                                } else if has_method && has_id {
                                    // A server-initiated REQUEST (method + id): its
                                    // id may collide with ours, so NEVER touch
                                    // `pending`. Reply directly on stdin.
                                    let method = v
                                        .get("method")
                                        .and_then(|m| m.as_str())
                                        .unwrap_or("");
                                    let id = v.get("id").cloned().unwrap_or(Value::Null);
                                    let reply = if method == "ping" {
                                        json!({ "jsonrpc": "2.0", "id": id, "result": {} })
                                    } else {
                                        json!({
                                            "jsonrpc": "2.0",
                                            "id": id,
                                            "error": {
                                                "code": -32601,
                                                "message": "method not found"
                                            }
                                        })
                                    };
                                    let line = serde_json::to_string(&reply).unwrap_or_else(|_| {
                                        r#"{"jsonrpc":"2.0","error":{"code":-32601,"message":"method not found"}}"#
                                            .to_string()
                                    });
                                    write_raw_line(&reader_name, &reader_stdin, &line).await;
                                } else if has_method {
                                    // A server-initiated NOTIFICATION (method, no id).
                                    let method = v
                                        .get("method")
                                        .and_then(|m| m.as_str())
                                        .unwrap_or("");
                                    if method == "notifications/tools/list_changed" {
                                        let _ =
                                            reader_tx.send(ClientEvent::ToolsListChanged);
                                    }
                                }
                            }
                            Err(e) => {
                                log(&format!(
                                    "[jack:{}] non-JSON stdout line ({}): {}",
                                    reader_name, e, trimmed
                                ));
                            }
                        }
                    }
                    Err(e) => {
                        log(&format!("[jack:{}] stdout read error: {}", reader_name, e));
                        drain_pending(&pending, format!("read error: {}", e));
                        let _ = reader_tx
                            .send(ClientEvent::Exited(format!("read error: {}", e)));
                        break;
                    }
                }
            }
        });

        client.tasks.lock().await.extend([stderr_task, reader_task]);

        Ok((client, event_rx))
    }

    /// Send a request and await its JSON-RPC `result`. Returns the `result`
    /// field on success, or `Err` for a timeout / transport failure / upstream
    /// JSON-RPC error.
    async fn request(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
    ) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        // Omit `params` when absent — a strict JSON-RPC peer rejects `"params":null`.
        let mut req = json!({ "jsonrpc": "2.0", "id": id, "method": method });
        if let Some(p) = params {
            req["params"] = p;
        }
        let line = serde_json::to_string(&req).map_err(|e| format!("serialize: {}", e))?;

        // Register the response channel BEFORE writing, so a blazing-fast reply
        // can't arrive with no waiter.
        let (tx, mut rx) = mpsc::channel::<Result<Value, String>>(1);
        self.pending.lock().insert(id, tx);

        {
            let mut guard = self.stdin.lock().await;
            let write_res: Result<(), String> = match guard.as_mut() {
                Some(stdin) => {
                    if let Err(e) = stdin.write_all(line.as_bytes()).await {
                        Err(format!("write: {}", e))
                    } else if let Err(e) = stdin.write_all(b"\n").await {
                        Err(format!("write: {}", e))
                    } else if let Err(e) = stdin.flush().await {
                        Err(format!("flush: {}", e))
                    } else {
                        Ok(())
                    }
                }
                None => Err("stdin closed (upstream shut down)".to_string()),
            };
            if let Err(e) = write_res {
                // (FIX 8) A wedged/closed pipe: drop our pending entry so it
                // doesn't leak in the map (a late reply would never match).
                self.pending.lock().remove(&id);
                return Err(e);
            }
        }

        // Await the response with a timeout.
        match tokio::time::timeout(timeout, rx.recv()).await {
            Ok(Some(Ok(resp))) => {
                // Upstream JSON-RPC error object -> surface as Err.
                if resp.get("error").is_some() {
                    return Err(format!("upstream error: {}", resp));
                }
                Ok(resp.get("result").cloned().unwrap_or(Value::Null))
            }
            Ok(Some(Err(e))) => Err(e),
            Ok(None) => Err("upstream dropped response channel".to_string()),
            Err(_) => {
                // Timeout: remove our pending entry so a late reply finds no waiter.
                self.pending.lock().remove(&id);
                Err(format!("upstream timeout after {:?}", timeout))
            }
        }
    }

    /// Send a notification (no `id`, no response expected).
    async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), String> {
        let mut msg = json!({ "jsonrpc": "2.0", "method": method });
        if let Some(p) = params {
            msg["params"] = p;
        }
        let line = serde_json::to_string(&msg).map_err(|e| format!("serialize: {}", e))?;
        let mut guard = self.stdin.lock().await;
        let stdin = guard
            .as_mut()
            .ok_or_else(|| "stdin closed (upstream shut down)".to_string())?;
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| format!("write: {}", e))?;
        stdin
            .write_all(b"\n")
            .await
            .map_err(|e| format!("write: {}", e))?;
        stdin.flush().await.map_err(|e| format!("flush: {}", e))?;
        Ok(())
    }

    /// MCP handshake: `initialize` with a minimal clientInfo + the preferred
    /// protocol version, then `notifications/initialized`.
    pub async fn initialize(&self) -> Result<(), String> {
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {
                "name": "patchbay",
                "version": env!("CARGO_PKG_VERSION"),
            }
        });
        self.request("initialize", Some(params), HANDSHAKE_TIMEOUT)
            .await?;
        self.notify("notifications/initialized", None).await?;
        Ok(())
    }

    /// Fetch the upstream's tool list. Returns the raw tool definitions (their
    /// names are NOT yet namespaced — namespacing is the gateway's job).
    pub async fn list_tools(&self) -> Result<Vec<Value>, String> {
        let resp = self
            .request("tools/list", Some(json!({})), CALL_TIMEOUT)
            .await?;
        let tools_val = resp.get("tools").cloned().unwrap_or(Value::Array(vec![]));
        let tools: Vec<Value> = serde_json::from_value(tools_val)
            .map_err(|e| format!("tools/list parse: {}", e))?;
        Ok(tools)
    }

    /// Forward a `tools/call` to the upstream child. Returns the upstream's
    /// `result` object (the `CallToolResult`, including any `isError` it set).
    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, String> {
        let params = json!({ "name": name, "arguments": arguments });
        self.request("tools/call", Some(params), CALL_TIMEOUT).await
    }

    /// Best-effort graceful shutdown: try `shutdown`, then close stdin + kill the
    /// child + abort the background tasks. The Job Object is the backstop for
    /// anything that resists.
    pub async fn shutdown(&self) {
        log(&format!("[jack:{}] shutdown requested", self.name));

        // (FIX 7) There is no MCP `shutdown` method; the old best-effort request
        // + notification stalled every unpatch/quit for up to 2 s. Just close
        // stdin (-> EOF), briefly wait, then kill + reap.
        {
            let mut guard = self.stdin.lock().await;
            *guard = None;
        }
        // Give the child a brief moment to exit gracefully on EOF.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Kill + reap the child.
        {
            let mut guard = self.child.lock().await;
            if let Some(mut child) = guard.take() {
                let _ = child.kill().await;
                let _ = child.wait().await;
            }
        }
        // Abort the background tasks (reader already finishing on EOF, stderr too).
        let tasks = std::mem::take(&mut *self.tasks.lock().await);
        for t in tasks {
            t.abort();
        }
        // Any callers still waiting get a clean failure rather than a hang.
        drain_pending(&self.pending, "shutdown");
        // Explicitly signal Exited so the manager's supervisor (which holds a
        // clone of this client and would otherwise keep the event channel open)
        // breaks out even if the reader was aborted before it processed EOF.
        let _ = self.event_tx.send(ClientEvent::Exited("shutdown".to_string()));
    }
}

/// Write one raw JSON-RPC line to the child's stdin, best-effort. Used to reply
/// to server-initiated requests (`ping`, `roots/list`, ...) without disturbing
/// the pending map. A closed/wedged pipe is logged and ignored.
async fn write_raw_line(
    name: &str,
    stdin: &Arc<TokioMutex<Option<ChildStdin>>>,
    json: &str,
) {
    let mut guard = stdin.lock().await;
    match guard.as_mut() {
        Some(stdin) => {
            if let Err(e) = stdin.write_all(json.as_bytes()).await {
                log(&format!("[jack:{}] reply write error: {}", name, e));
                return;
            }
            if let Err(e) = stdin.write_all(b"\n").await {
                log(&format!("[jack:{}] reply write error: {}", name, e));
                return;
            }
            if let Err(e) = stdin.flush().await {
                log(&format!("[jack:{}] reply flush error: {}", name, e));
            }
        }
        None => {}
    }
}

/// Remove and fail every pending request with `reason` (child death / shutdown).
fn drain_pending<E>(pending: &Arc<parking_lot::Mutex<HashMap<i64, PendingTx>>>, reason: E)
where
    E: Into<String>,
{
    let reason = reason.into();
    let drained = std::mem::take(&mut *pending.lock());
    // Spawned off the async lock; sending is async on an mpsc::Sender.
    for (_, tx) in drained {
        // try_send: best-effort; the receiver may already be gone (timeout path).
        let _ = tx.try_send(Err(reason.clone()));
    }
}

// ---- UpstreamClient (transport-agnostic trait used by UpstreamManager) ----
//
// The inherent methods above are kept as-is; this impl delegates to them so the
// manager can hold `Arc<dyn UpstreamClient>` uniformly across stdio + http. We
// call via `self.<method>()` which resolves to the INHERENT method (inherent
// candidates shadow trait candidates on the concrete `StdioClient` self), so
// there is no ambiguity with the trait method of the same name.
#[async_trait::async_trait]
impl UpstreamClient for StdioClient {
    async fn initialize(&self) -> Result<(), String> {
        self.initialize().await
    }
    async fn list_tools(&self) -> Result<Vec<Value>, String> {
        self.list_tools().await
    }
    async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, String> {
        self.call_tool(name, arguments).await
    }
    async fn shutdown(&self) {
        self.shutdown().await
    }
}
