//! A connected Streamable HTTP MCP client: one shared `reqwest::Client` per jack
//! (MASTER_PLAN S6 / D4).
//!
//! Mirrors [`crate::upstream::stdio::StdioClient`] semantics over HTTP:
//! - `connect` builds the `reqwest::Client` and the supervisor event channel but
//!   performs NO network I/O (the handshake is a separate `initialize` call, just
//!   like stdio's spawn does not initialize).
//! - `initialize` POSTs the JSON-RPC `initialize` request with
//!   `Accept: application/json, text/event-stream`, captures the server's
//!   `Mcp-Session-Id` response header, replays it on every subsequent request,
//!   then POSTs `notifications/initialized`.
//! - `list_tools` / `call_tool` POST the JSON-RPC request. The response is either
//!   a single `application/json` JSON-RPC envelope, OR `text/event-stream` — in
//!   the SSE case the events are drained until the JSON-RPC response carrying the
//!   matching `id` arrives, then the stream is dropped. The `result` field is
//!   returned (or `Err` on a JSON-RPC error / transport failure).
//! - `shutdown` flips a `closed` flag (so no further outbound requests are made —
//!   enforcement: unpatched ⇒ no reachability) and best-effort DELETEs the upstream
//!   session endpoint if the server issued a session id.
//!
//! Headers are decrypted at connect time via `config::secrets::decrypted_headers`
//! and held for the connection lifetime (they must be sent on every request); they
//! are never persisted in plaintext (anything on disk is `dpapi:`-wrapped).
//!
//! v0.1 limitation (documented, best-effort): we do NOT hold a long-lived GET
//! stream to the upstream, so a server-initiated
//! `notifications/tools/list_changed` is only surfaced if it happens to arrive on
//! a POST response stream. The important guarantee — an UNPATCHED http jack sends
//! NOTHING outbound — is enforced by the gateway checking `patched` before routing
//! plus `stop_jack` dropping the client.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::sync::{mpsc, Mutex as TokioMutex};

use reqwest::header::{HeaderName, HeaderValue};

use crate::upstream::client::{ClientEvent, UpstreamClient};
use crate::utils::log::log;

/// MCP protocol version we offer upstreams (MASTER_PLAN D4 — same as stdio).
const PROTOCOL_VERSION: &str = "2025-06-18";
/// Handshake (`initialize`) timeout.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// Per-call (`tools/list`, `tools/call`) timeout.
const CALL_TIMEOUT: Duration = Duration::from_secs(120);
/// Notification (`notifications/initialized`) + session DELETE timeout.
const NOTIFY_TIMEOUT: Duration = Duration::from_secs(30);

/// One connected Streamable HTTP MCP server. Cheaply shared behind an `Arc`.
pub struct HttpClient {
    /// Jack name, used purely for log prefixes.
    name: String,
    /// Upstream Streamable HTTP endpoint (e.g. `http://127.0.0.1:PORT/mcp`).
    url: String,
    /// The shared HTTP client (connection-pooled; built once at connect).
    http: reqwest::Client,
    /// Decrypted request headers (e.g. `Authorization`), sent on every request.
    headers: BTreeMap<String, String>,
    /// Server-issued `Mcp-Session-Id`, captured from the `initialize` response
    /// and replayed on every subsequent request.
    session_id: Mutex<Option<String>>,
    /// Monotonic source of fresh upstream-local request ids.
    next_id: AtomicI64,
    /// Set by `shutdown`; once true every outbound request fails fast so an
    /// unpatched jack can never reach the upstream.
    closed: AtomicBool,
    /// Set after a successful `initialize` so subsequent requests carry the
    /// negotiated `MCP-Protocol-Version` header (FIX 12b; some servers require it).
    initialized: AtomicBool,
    /// Serializes reinitialize attempts (FIX: stale session auto-recovery) so a
    /// burst of concurrent calls hitting a stale session don't each fire their
    /// own `initialize` — the first waiter reinitializes, the rest see the
    /// fresh `session_id` and skip straight to retrying their own request.
    reinit_lock: TokioMutex<()>,
    /// Signals to the manager's supervisor (list_changed / exit).
    event_tx: mpsc::UnboundedSender<ClientEvent>,
}

impl HttpClient {
    /// Build the `reqwest::Client` + supervisor event channel. Performs NO
    /// network I/O — the handshake is a separate `initialize()` call, mirroring
    /// stdio's spawn-then-initialize split.
    ///
    /// `headers` must already be **decrypted** (caller runs
    /// `config::secrets::decrypted_headers(jack)` at connect, per MASTER_PLAN D4).
    pub fn connect(
        name: String,
        url: String,
        headers: BTreeMap<String, String>,
    ) -> (Arc<Self>, mpsc::UnboundedReceiver<ClientEvent>) {
        // (FIX 12a) Never follow redirects: an upstream that 30x's would
        // otherwise replay our auth headers (Authorization / session id) to an
        // untrusted destination.
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_else(|e| {
                log(&format!(
                    "[jack:{}] reqwest builder failed ({}); falling back to no-redirect default client",
                    name, e
                ));
                reqwest::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
                    .unwrap_or_else(|_| reqwest::Client::new())
            });
        let (event_tx, event_rx) = mpsc::unbounded_channel::<ClientEvent>();
        let client = Arc::new(HttpClient {
            name,
            url,
            http,
            headers,
            session_id: Mutex::new(None),
            next_id: AtomicI64::new(1),
            closed: AtomicBool::new(false),
            initialized: AtomicBool::new(false),
            reinit_lock: TokioMutex::new(()),
            event_tx,
        });
        (client, event_rx)
    }

    /// Attach the decrypted headers + the current session id to a request
    /// builder. Invalid header names/values are logged and skipped (never panic).
    /// Values are inserted as `&str` — the well-trodden path into reqwest's
    /// generic `.header` (both `HeaderName` and `HeaderValue` impl `TryFrom<&str>`).
    fn with_shared_headers(
        &self,
        mut builder: reqwest::RequestBuilder,
    ) -> reqwest::RequestBuilder {
        for (k, v) in &self.headers {
            // Validate so a bad header is logged + skipped, not a request error.
            if HeaderName::from_bytes(k.as_bytes()).is_err() {
                log(&format!(
                    "[jack:{}] skipping header with invalid name '{}'",
                    self.name, k
                ));
                continue;
            }
            if HeaderValue::from_str(v).is_err() {
                log(&format!(
                    "[jack:{}] skipping header '{}' with invalid value",
                    self.name, k
                ));
                continue;
            }
            builder = builder.header(k.as_str(), v.as_str());
        }
        if let Some(sid) = self.session_id.lock().clone() {
            builder = builder.header("mcp-session-id", sid.as_str());
        }
        // (FIX 12b) Once initialized, send the negotiated protocol version on
        // every request (some Streamable HTTP servers reject calls without it).
        if self.initialized.load(Ordering::SeqCst) {
            builder = builder.header("mcp-protocol-version", PROTOCOL_VERSION);
        }
        builder
    }

    /// POST a JSON-RPC request and return its `result` field (or `Err` on a
    /// JSON-RPC error / transport failure). Handles both `application/json` and
    /// `text/event-stream` responses.
    ///
    /// Auto-recovers from a stale `Mcp-Session-Id` (FIX: the upstream — e.g. a
    /// restarted tabduct hub — forgot our session): a `SessionInvalid` on
    /// anything but `initialize` itself triggers one transparent
    /// reinitialize-then-retry before the caller ever sees an error. This is
    /// invisible from the gateway/tray's point of view: a jack that looks
    /// "Running" keeps working across an upstream restart instead of failing
    /// every call forever until the user manually toggles it.
    async fn request(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
    ) -> Result<Value, String> {
        match self.send_once(method, params.clone(), timeout).await {
            Ok(v) => Ok(v),
            Err(ReqErr::SessionInvalid) if method != "initialize" => {
                self.reinitialize_then_retry(method, params, timeout).await
            }
            Err(e) => Err(e.to_string()),
        }
    }

    /// Reinitialize (deduped via `reinit_lock` — a burst of concurrent callers
    /// all hitting the same stale session only reinitializes once) and retry
    /// the original request exactly once. If the retry hits `SessionInvalid`
    /// again, that's a real failure (not just a stale-session race) and is
    /// surfaced as-is rather than looping.
    async fn reinitialize_then_retry(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
    ) -> Result<Value, String> {
        {
            let _guard = self.reinit_lock.lock().await;
            // Another waiter may have already reinitialized while we queued for
            // the lock — only do it ourselves if the session is still empty.
            if self.session_id.lock().is_none() {
                log(&format!(
                    "[jack:{}] stale session detected; reinitializing",
                    self.name
                ));
                self.do_initialize().await?;
            }
        }
        self.send_once(method, params, timeout)
            .await
            .map_err(|e| e.to_string())
    }

    /// The actual handshake logic (params + `initialize` + `notifications/initialized`),
    /// shared by the initial connect and by stale-session recovery. Clears the
    /// old session id first so a failed attempt never resends an id we know is dead.
    async fn do_initialize(&self) -> Result<(), String> {
        *self.session_id.lock() = None;
        self.initialized.store(false, Ordering::SeqCst);
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {
                "name": "patchbay",
                "version": env!("CARGO_PKG_VERSION"),
            }
        });
        // Bypass `request()`'s own retry wrapper (would recurse): "initialize"
        // is already excluded there, but call send_once directly for clarity.
        self.send_once("initialize", Some(params), HANDSHAKE_TIMEOUT)
            .await
            .map_err(|e| e.to_string())?;
        self.notify("notifications/initialized", None).await?;
        self.initialized.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// One HTTP round-trip for a JSON-RPC request — no retry, no reinitialize.
    /// Returns [`ReqErr::SessionInvalid`] when the upstream's response is
    /// unmistakably "your `Mcp-Session-Id` is gone" (HTTP 400 + JSON-RPC error
    /// code -32000, the code the MCP spec reserves for this), so the caller can
    /// decide whether a retry is appropriate; any other non-success status is
    /// [`ReqErr::Other`].
    async fn send_once(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
    ) -> Result<Value, ReqErr> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(ReqErr::Other("http upstream shut down".to_string()));
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        // Omit `params` entirely when there are none — a strict JSON-RPC server
        // (e.g. the MCP SDK's Streamable HTTP transport) rejects `"params":null`.
        let mut body = json!({ "jsonrpc": "2.0", "id": id, "method": method });
        if let Some(p) = params {
            body["params"] = p;
        }
        let builder = self
            .http
            .post(&self.url)
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .json(&body);
        let builder = self.with_shared_headers(builder);

        let resp = tokio::time::timeout(timeout, builder.send())
            .await
            .map_err(|_| ReqErr::Other(format!("upstream timeout after {:?}", timeout)))?
            .map_err(|e| ReqErr::Other(format!("http request: {}", e)))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            // (Review fix, live-diagnosed) Different real MCP servers use
            // different HTTP status codes for "your session is gone": tabduct
            // replies 400, eUnifyMCP-Test/-Prod reply 404 (actually the MCP
            // spec's own recommended code — see the gateway's OWN dead-session
            // handling in gateway/http.rs). Gating on `== BAD_REQUEST` alone
            // meant this whole auto-recovery path was UNREACHABLE for any
            // server using 404, regardless of how well `is_session_invalid_body`
            // matched the body — confirmed live: eUnifyMCP-Test/-Prod session
            // loss surfaced as a raw "upstream HTTP 404" failure with zero
            // auto-recovery, while tabduct (400) always self-healed. A
            // session-invalid signal is inherently a client-facing error, so
            // check the body across the WHOLE 4xx range rather than pin to one
            // specific code.
            if status.is_client_error() && is_session_invalid_body(&text) {
                // Clear the now-dead id immediately (not just inside
                // `do_initialize`): `reinitialize_then_retry`'s dedup guard
                // checks `session_id.lock().is_none()` to decide whether a
                // reinitialize is still needed. Without clearing it here, the
                // guard would see the OLD (stale) id, conclude nothing needs
                // to happen, and retry with the exact same dead id forever.
                *self.session_id.lock() = None;
                return Err(ReqErr::SessionInvalid);
            }
            return Err(ReqErr::Other(format!(
                "upstream HTTP {}: {}",
                status,
                truncate(&text, 200)
            )));
        }

        // Capture / refresh the server-issued session id (first seen on the
        // initialize response; some servers echo it on every response).
        if let Some(sid) = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|h| h.to_str().ok())
        {
            *self.session_id.lock() = Some(sid.to_string());
        }

        let is_sse = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|c| c.contains("text/event-stream"))
            .unwrap_or(false);

        let envelope = if is_sse {
            self.read_sse_until(resp, id, timeout)
                .await
                .map_err(ReqErr::Other)?
        } else {
            // `application/json` (or unknown): exactly one JSON-RPC response.
            let text = resp
                .text()
                .await
                .map_err(|e| ReqErr::Other(format!("read body: {}", e)))?;
            serde_json::from_str::<Value>(&text).map_err(|e| {
                ReqErr::Other(format!(
                    "response JSON parse: {} (body: {})",
                    e,
                    truncate(&text, 200)
                ))
            })?
        };

        if envelope.get("error").is_some() {
            return Err(ReqErr::Other(format!("upstream error: {}", envelope)));
        }
        Ok(envelope.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Classify one SSE event: returns `Some(envelope)` if it is the JSON-RPC
    /// response carrying `want_id`, else `None` (after best-effort surfacing of a
    /// `notifications/tools/list_changed` notification). Used by both the
    /// streaming loop and the EOF tail in [`read_sse_until`].
    fn process_event(&self, event: &str, want_id: i64) -> Option<Value> {
        let data = parse_event_data(event)?;
        match serde_json::from_str::<Value>(&data) {
            Ok(v) => {
                // (FIX 12c) The response id may be a number OR a string equal to
                // our id (some servers echo ids as strings).
                let id_matches = v
                    .get("id")
                    .map(|i| {
                        i.as_i64() == Some(want_id)
                            || i.as_str() == Some(&want_id.to_string())
                    })
                    .unwrap_or(false);
                if id_matches {
                    return Some(v);
                }
                // Not our response: maybe a server notification. Surface
                // list_changed best-effort (see the module-level v0.1 note).
                if v.get("method").and_then(|m| m.as_str())
                    == Some("notifications/tools/list_changed")
                {
                    let _ = self.event_tx.send(ClientEvent::ToolsListChanged);
                }
                None
            }
            Err(e) => {
                log(&format!(
                    "[jack:{}] non-JSON SSE data line ({}): {}",
                    self.name,
                    e,
                    truncate(&data, 200)
                ));
                None
            }
        }
    }

    /// Drain an SSE response stream until the JSON-RPC response carrying `want_id`
    /// arrives, then return that full envelope (and stop reading). Any
    /// `notifications/tools/list_changed` seen along the way is surfaced as a
    /// [`ClientEvent`] (best-effort; see the module-level v0.1 note).
    async fn read_sse_until(
        &self,
        resp: reqwest::Response,
        want_id: i64,
        timeout: Duration,
    ) -> Result<Value, String> {
        use futures::StreamExt;
        let mut stream = resp.bytes_stream();
        let mut buf = String::new();
        loop {
            let next = tokio::time::timeout(timeout, stream.next()).await;
            match next {
                Err(_) => return Err(format!("upstream SSE timeout after {:?}", timeout)),
                Ok(None) => {
                    // Stream ended. Process any trailing event that the server
                    // sent without a final blank line before closing.
                    if !buf.is_empty() {
                        let trailing = std::mem::take(&mut buf);
                        if let Some(v) = self.process_event(&trailing, want_id) {
                            return Ok(v);
                        }
                    }
                    break;
                }
                Ok(Some(Err(e))) => return Err(format!("sse read: {}", e)),
                Ok(Some(Ok(bytes))) => {
                    // Normalize CRLF -> LF so both `\n\n` and `\r\n\r\n`
                    // event boundaries reduce to `\n\n`.
                    let chunk = String::from_utf8_lossy(&bytes).replace("\r\n", "\n");
                    buf.push_str(&chunk);
                    // Peel off every complete event (terminated by a blank line).
                    while let Some(pos) = buf.find("\n\n") {
                        let event = buf[..pos].to_string();
                        buf.drain(..pos + 2);
                        if let Some(v) = self.process_event(&event, want_id) {
                            return Ok(v);
                        }
                    }
                }
            }
        }
        Err(format!(
            "upstream closed response stream without a result for id {}",
            want_id
        ))
    }

    /// POST a JSON-RPC notification (no `id`, no response body expected). The
    /// server typically answers `202 Accepted`.
    async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), String> {
        if self.closed.load(Ordering::SeqCst) {
            return Err("http upstream shut down".to_string());
        }
        // Omit `params` when absent (strict JSON-RPC servers reject `null`).
        let mut msg = json!({ "jsonrpc": "2.0", "method": method });
        if let Some(p) = params {
            msg["params"] = p;
        }
        let builder = self
            .http
            .post(&self.url)
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .json(&msg);
        let builder = self.with_shared_headers(builder);

        let resp = tokio::time::timeout(NOTIFY_TIMEOUT, builder.send())
            .await
            .map_err(|_| format!("upstream notify timeout after {:?}", NOTIFY_TIMEOUT))?
            .map_err(|e| format!("http notify: {}", e))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("upstream notify HTTP {}", status));
        }
        // A server may issue/echo the session id on any response.
        if let Some(sid) = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|h| h.to_str().ok())
        {
            *self.session_id.lock() = Some(sid.to_string());
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl UpstreamClient for HttpClient {
    async fn initialize(&self) -> Result<(), String> {
        self.do_initialize().await
    }

    async fn list_tools(&self) -> Result<Vec<Value>, String> {
        let resp = self
            .request("tools/list", Some(json!({})), CALL_TIMEOUT)
            .await?;
        let tools_val = resp.get("tools").cloned().unwrap_or(Value::Array(vec![]));
        let tools: Vec<Value> = serde_json::from_value(tools_val)
            .map_err(|e| format!("tools/list parse: {}", e))?;
        Ok(tools)
    }

    async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, String> {
        let params = json!({ "name": name, "arguments": arguments });
        self.request("tools/call", Some(params), CALL_TIMEOUT).await
    }

    async fn shutdown(&self) {
        log(&format!("[jack:{}] shutdown requested", self.name));
        // Flip closed FIRST so any in-flight or future request fails fast — an
        // unpatched jack must make no further outbound requests.
        self.closed.store(true, Ordering::SeqCst);
        // Best-effort session termination: DELETE the upstream endpoint with the
        // session id, if the server issued one.
        let sid = self.session_id.lock().take();
        if let Some(sid) = sid {
            let builder = self.http.delete(&self.url);
            let builder = self
                .with_shared_headers(builder)
                .header("mcp-session-id", sid.as_str());
            let _ = tokio::time::timeout(Duration::from_secs(3), builder.send()).await;
        }
        // Tell the manager's supervisor we're done (it holds a client clone and
        // would otherwise block on events forever).
        let _ = self
            .event_tx
            .send(ClientEvent::Exited("shutdown".to_string()));
    }
}

/// Distinguishes "the upstream doesn't recognize our `Mcp-Session-Id`" (worth
/// auto-recovering from — reinitialize + retry) from any other HTTP/JSON-RPC
/// failure (a real error, surfaced to the caller as-is).
enum ReqErr {
    SessionInvalid,
    Other(String),
}

impl std::fmt::Display for ReqErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReqErr::SessionInvalid => write!(f, "upstream session invalid"),
            ReqErr::Other(s) => write!(f, "{}", s),
        }
    }
}

/// Is this HTTP 400 body the MCP "no/unknown session" error? Matched on the
/// JSON-RPC error code the spec reserves for it (-32000) when parseable, with
/// a substring fallback for servers that reply with a similar but not
/// byte-for-byte-identical envelope (matches tabduct's
/// `hosts/node/src/mcp-server.js` "No valid session; initialize first").
fn is_session_invalid_body(text: &str) -> bool {
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        if let Some(code) = v.get("error").and_then(|e| e.get("code")).and_then(|c| c.as_i64()) {
            // -32000..=-32099 is the JSON-RPC spec's reserved "Server error"
            // range — different real MCP servers pick different codes within
            // it for "your session is gone" (tabduct uses -32000; eUnifyMCP
            // uses -32001; observed live, they disagree on the exact code).
            // Combined with the message mentioning "session" so we don't
            // accidentally treat every implementation-defined server error as
            // a session issue (a code in this range with an unrelated message
            // — e.g. a genuine internal error — must NOT trigger a
            // reinitialize loop).
            if (-32099..=-32000).contains(&code) {
                let msg_lower = v
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if msg_lower.contains("session") {
                    return true;
                }
            }
        }
    }
    // Fallback for servers that don't reply with a byte-for-byte JSON-RPC
    // error envelope (e.g. tabduct's mcp-server.js sends a plain-text body
    // for this specific case): broadened beyond the original
    // "initialize first"/"no valid" phrasing to also catch "session not
    // found"/"session invalid"/"session expired" — the wording real servers
    // actually use varies more than any fixed phrase list can fully cover, so
    // this stays a best-effort net, not the primary signal (the JSON-RPC code
    // check above is).
    let lower = text.to_ascii_lowercase();
    lower.contains("session")
        && (lower.contains("initialize first")
            || lower.contains("no valid")
            || lower.contains("not found")
            || lower.contains("invalid")
            || lower.contains("expired"))
}

// ---- SSE parsing helpers ----------------------------------------------------

/// From one SSE event block, join its `data:` lines (per the SSE spec, multiple
/// `data:` fields are concatenated with `\n`). Returns `None` if the event
/// carries no data.
fn parse_event_data(event: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for line in event.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            // An optional single leading space after the colon is stripped.
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            parts.push(rest);
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

/// Truncate a string to `n` chars for log/error context (char-boundary safe).
fn truncate(s: &str, n: usize) -> String {
    let taken: String = s.chars().take(n).collect();
    if taken.chars().count() < s.chars().count() {
        format!("{}...", taken)
    } else {
        taken
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_event_data_joins_multiple_data_lines() {
        // A JSON-RPC response split across two `data:` lines must be rejoined
        // with `\n` per the SSE spec. (server-everything sends single-line, but
        // be spec-correct.)
        let event = "event: message\ndata: {\"jsonrpc\":\"2.0\",\ndata: \"id\":1}";
        let data = parse_event_data(event).unwrap();
        assert_eq!(data, "{\"jsonrpc\":\"2.0\",\n\"id\":1}");
        let v: Value = serde_json::from_str(&data).unwrap();
        assert_eq!(v["id"], 1);
    }

    #[test]
    fn parse_event_data_none_when_no_data_field() {
        let event = "event: ping\n:comment line\n";
        assert!(parse_event_data(event).is_none());
    }

    #[test]
    fn parse_event_data_strips_optional_leading_space() {
        let event = "data:{\"ok\":true}";
        assert_eq!(parse_event_data(event).unwrap(), "{\"ok\":true}");
    }

    #[test]
    fn is_session_invalid_body_matches_jsonrpc_dash32000() {
        let body = r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32000,"message":"Mcp-Session-Id not found"}}"#;
        assert!(is_session_invalid_body(body));
    }

    #[test]
    fn is_session_invalid_body_matches_tabduct_style_text_fallback() {
        // tabduct's mcp-server.js replies with this phrasing rather than a
        // byte-for-byte JSON-RPC -32000 envelope.
        assert!(is_session_invalid_body("No valid session; initialize first"));
        assert!(is_session_invalid_body("no valid session found, please initialize first"));
    }

    #[test]
    fn is_session_invalid_body_matches_eunifymcp_dash32001() {
        // eUnifyMCP-Test's exact observed error (live, 2026-07-12): a
        // DIFFERENT code from tabduct's -32000, which the original
        // hardcoded-to-32000 check missed entirely — this jack never
        // auto-recovered until the check was widened to the whole
        // -32000..=-32099 "Server error" range.
        let body = r#"{"error":{"code":-32001,"message":"Session not found"},"id":"","jsonrpc":"2.0"}"#;
        assert!(is_session_invalid_body(body));
    }

    #[test]
    fn is_session_invalid_body_text_fallback_covers_more_phrasings() {
        assert!(is_session_invalid_body("Session not found"));
        assert!(is_session_invalid_body("session invalid"));
        assert!(is_session_invalid_body("Your session has expired"));
    }

    #[test]
    fn is_session_invalid_body_rejects_unrelated_errors() {
        // Reserved server-error-range code, but the message has nothing to do
        // with sessions — must NOT trigger a reinitialize loop.
        assert!(!is_session_invalid_body(r#"{"error":{"code":-32050,"message":"internal database error"}}"#));
        // Outside the reserved server-error range entirely.
        assert!(!is_session_invalid_body(r#"{"error":{"code":-32602,"message":"invalid params"}}"#));
        assert!(!is_session_invalid_body("internal server error"));
        assert!(!is_session_invalid_body(""));
    }

    #[test]
    fn truncate_is_char_safe_and_marks_truncation() {
        assert_eq!(truncate("hello world", 5), "hello...");
        assert_eq!(truncate("abc", 5), "abc");
        // Multibyte: must not panic on a char boundary.
        let _ = truncate("héllo", 2);
    }
}
