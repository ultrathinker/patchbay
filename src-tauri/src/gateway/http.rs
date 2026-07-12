//! axum HTTP handlers for `/mcp` (Streamable HTTP transport, MASTER_PLAN D4).
//!
//! - `POST /mcp`: parse JSON-RPC; `initialize` needs no session id and sets the
//!   `Mcp-Session-Id` response header; every other method REQUIRES a valid
//!   `Mcp-Session-Id` (else 400). Notifications (id absent) -> 202 empty;
//!   requests -> 200 + JSON-RPC body.
//! - `GET /mcp`: SSE — the canonical per-session notification stream (D3).
//!   Requires a valid `Mcp-Session-Id`; honors `Last-Event-ID` replay; keepalive
//!   comments ~20 s; generation-guarded unregister on disconnect.
//! - `DELETE /mcp`: require `Mcp-Session-Id`, drop the session, return 204.
//!
//! An [`origin_guard`] middleware rejects any non-localhost `Origin` (absent
//! Origin = CLI client = allowed). Every inbound method + outbound status is
//! logged via `utils::log` for client-interop debugging.

use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::sse::{KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures::StreamExt;
use serde_json::json;
use tokio_stream::wrappers::ReceiverStream;

use crate::app_state::AppState;
use crate::gateway::handlers::{self, DispatchOutcome};
use crate::gateway::jsonrpc;
use crate::gateway::session::SseRecord;
use crate::gateway::sse;
use crate::utils::log::log;

/// The `Mcp-Session-Id` header, as a lowercase &'static str (HeaderMap lookups
/// accept `&str`; inserts use `HeaderName::from_static`).
const MCP_SESSION_ID: &str = "mcp-session-id";

/// (S10b) Optional user-chosen client-identity header. `HeaderMap` lookups are
/// case-insensitive, so the lowercase form matches any casing a client sends
/// (`X-Patchbay-Client`, `x-patchbay-client`, …). When present and non-empty
/// (after trimming) it takes PRIORITY over the connecting agent's self-reported
/// `clientInfo.name`, letting the user label/distinguish agents (e.g.
/// "claude-work" vs "claude-personal") from the agent's own connection config.
/// Absent/empty -> graceful fallback to `clientInfo.name` (S10 behavior).
const PATCHBAY_CLIENT_HEADER: &str = "x-patchbay-client";

// ---- origin validation middleware ----------------------------------------

/// Reject requests whose `Origin` host is not 127.0.0.1 / localhost / [::1].
/// Absent `Origin` (typical CLI clients) is allowed through.
pub async fn origin_guard(request: Request, next: Next) -> Response {
    let origin = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    if let Some(ref origin) = origin {
        if !is_localhost_origin(origin) {
            log(&format!("http: rejecting non-localhost origin: {}", origin));
            return (
                StatusCode::FORBIDDEN,
                "forbidden: Origin is not a localhost address",
            )
                .into_response();
        }
    }
    next.run(request).await
}

/// True if `origin` (an `Origin` header value like `http://127.0.0.1:39100`)
/// points at a localhost host.
fn is_localhost_origin(origin: &str) -> bool {
    // Strip scheme.
    let after_scheme = match origin.find("://") {
        Some(i) => &origin[i + 3..],
        None => origin,
    };
    // Authority ends at the first '/' (path), if any.
    let authority = after_scheme.split('/').next().unwrap_or(after_scheme);
    // Host is the authority up to its final ':' (handles [::1]:port).
    let host = authority.rsplit_once(':').map(|(h, _)| h).unwrap_or(authority);
    matches!(host, "127.0.0.1" | "localhost" | "[::1]")
}

// ---- POST /mcp -----------------------------------------------------------

/// Resolve the connecting client's identity for the Level-2 request log, using
/// the SAME resolution as dispatch: the `X-Patchbay-Client` header (trimmed,
/// non-empty) takes PRIORITY over `clientInfo.name` on `initialize`; every other
/// method uses the name cached on the session at initialize time. `None` for an
/// unidentified client (header absent AND no clientInfo / no live session).
fn client_name_for_log(
    state: &AppState,
    req: &jsonrpc::JsonRpcRequest,
    headers: &HeaderMap,
) -> Option<String> {
    if req.method == "initialize" {
        let header = headers
            .get(PATCHBAY_CLIENT_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if let Some(h) = header {
            return Some(h.to_string());
        }
        req.params
            .as_ref()
            .and_then(|p| p.get("clientInfo"))
            .and_then(|ci| ci.get("name"))
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    } else {
        headers
            .get(MCP_SESSION_ID)
            .and_then(|v| v.to_str().ok())
            .and_then(|id| state.sessions.get(id))
            .and_then(|s| s.client_name.read().clone())
    }
}

pub async fn post_mcp(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // 1. Parse the inbound JSON-RPC request.
    let req: jsonrpc::JsonRpcRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            log(&format!("http: POST -> 400 parse error: {}", e));
            let resp = jsonrpc::error(None, jsonrpc::PARSE_ERROR, "parse error", None);
            return status_json(
                StatusCode::BAD_REQUEST,
                Some(&serde_json::to_value(resp).expect("serializable")),
                None,
            );
        }
    };

    let method = req.method.clone();
    let is_initialize = method == "initialize";

    // Level-2 request log: timestamp + resolved client identity + JSON-RPC
    // method + redacted headers + truncated body/params. Logged for EVERY
    // well-formed request (before dispatch, so it lands even for the
    // dead-session / model-directed-text paths below). No-op when request
    // logging is off (checked live on every write).
    {
        let client = client_name_for_log(&state, &req, &headers);
        crate::utils::request_log::log_request(
            &state,
            client.as_deref(),
            &method,
            &headers,
            req.params.as_ref(),
        );
    }

    // 2. Dispatch. initialize creates its own session; everything else needs a
    //    valid Mcp-Session-Id resolved to a live session.
    let outcome = if is_initialize {
        // (S10b) Extract the user-chosen identity header (trimmed; empty/
        // whitespace-only treated as absent) and give it PRIORITY over
        // clientInfo.name when the session's client identity is resolved in
        // handle_initialize. Looked up the same way Mcp-Session-Id is above.
        let header_client: Option<&str> = headers
            .get(PATCHBAY_CLIENT_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        handlers::dispatch(&state, &req, None, header_client).await
    } else {
        let header_val = headers.get(MCP_SESSION_ID).and_then(|v| v.to_str().ok());
        let session = header_val.and_then(|id| state.sessions.get(id));
        match session {
            Some(s) => handlers::dispatch(&state, &req, Some(s), None).await,
            // (dead/missing-session UX fix, D2) No live session resolves for
            // this request — either a session id WAS supplied but doesn't
            // resolve (expired, or Patchbay restarted and lost all sessions),
            // or none was sent at all (never called `initialize`). The
            // spec-correct signal is HTTP 404/400 (the `None` arm below,
            // still used for every method with no model-visible result
            // shape) — but in practice, several real MCP clients only
            // surface transport-level errors to their own retry/reconnect
            // logic and never let the MODEL see them, so a model that could
            // otherwise just call `initialize` again (or tell the user what
            // to do) never gets the chance: the client keeps silently
            // re-sending the same dead/absent session forever.
            //
            // `tools/call` and `tools/list` are worth special-casing because
            // their result IS something the model reads verbatim — a
            // CallToolResult's `content` for the former, and a tool's own
            // `name`/`description` (literally what tells the model what it
            // can do) for the latter. Instead of a transport error, return an
            // ordinary 200 JSON-RPC success carrying explicit, model-readable
            // instructions in that shape. This reaches the model through the
            // exact same path a normal result would, regardless of how
            // unsophisticated the client's own transport-error handling is.
            None if header_val.is_some() && (method == "tools/call" || method == "tools/list") => {
                log(&format!(
                    "http: POST {} -> 200 (dead session, model-directed reinitialize text)",
                    method
                ));
                let result = if method == "tools/call" {
                    dead_session_calltool_result(false)
                } else {
                    dead_session_toolslist_result(false)
                };
                let resp = jsonrpc::success(req.id.clone(), result);
                return status_json(
                    StatusCode::OK,
                    Some(&serde_json::to_value(resp).expect("serializable")),
                    None,
                );
            }
            None if header_val.is_some() => {
                // Every OTHER method (`ping`, `notifications/initialized`,
                // unknown methods) has no natural "model-readable result"
                // shape to hijack the way tools/call/tools/list do above, so
                // it keeps the spec-correct transport error: 404 is the
                // signal a well-behaved client watches for to automatically
                // start a fresh session via `initialize`.
                log(&format!(
                    "http: POST {} -> 404 (session not found/expired, client should reinitialize)",
                    method
                ));
                let resp = jsonrpc::error(
                    None,
                    jsonrpc::INVALID_REQUEST,
                    "session not found or expired — call 'initialize' again to start a fresh session (Patchbay likely restarted)",
                    None,
                );
                return status_json(
                    StatusCode::NOT_FOUND,
                    Some(&serde_json::to_value(resp).expect("serializable")),
                    None,
                );
            }
            None if method == "tools/call" || method == "tools/list" => {
                // No session id was sent AT ALL (not even a dead/expired one)
                // — this client never called `initialize` successfully before
                // trying to use a tool. Same model-readable-text principle as
                // the dead-session case above.
                log(&format!(
                    "http: POST {} -> 200 (no session yet, model-directed initialize text)",
                    method
                ));
                let result = if method == "tools/call" {
                    dead_session_calltool_result(true)
                } else {
                    dead_session_toolslist_result(true)
                };
                let resp = jsonrpc::success(req.id.clone(), result);
                return status_json(
                    StatusCode::OK,
                    Some(&serde_json::to_value(resp).expect("serializable")),
                    None,
                );
            }
            None => {
                log(&format!(
                    "http: POST {} -> 400 (missing Mcp-Session-Id)",
                    method
                ));
                let resp = jsonrpc::error(
                    None,
                    jsonrpc::INVALID_REQUEST,
                    "missing Mcp-Session-Id header",
                    None,
                );
                return status_json(
                    StatusCode::BAD_REQUEST,
                    Some(&serde_json::to_value(resp).expect("serializable")),
                    None,
                );
            }
        }
    };

    // 3. Build + log. (Notifications already carry 202/None from dispatch.)
    log(&format!("http: POST {} -> {}", method, outcome.http_status));
    build_response(&outcome)
}

/// Build the `tools/call` `CallToolResult` (D2) for a dead/missing session —
/// text the calling MODEL reads verbatim as this tool call's own output,
/// regardless of how the client's own transport-error handling behaves.
/// `never_initialized` distinguishes "no session id was sent at all" from "a
/// session id was sent but doesn't resolve" (worded slightly differently).
fn dead_session_calltool_result(never_initialized: bool) -> serde_json::Value {
    let text = if never_initialized {
        "You have not established an MCP session with Patchbay yet. Call 'initialize' first, then retry this tool call. If this is your very first connection to Patchbay, the user may need to approve access via a dialog on their screen before tools become available — after initializing, wait a few seconds for that approval, then retry."
    } else {
        "Your MCP session with Patchbay is no longer valid (it expired, or Patchbay restarted). This is not a tool failure. Call 'initialize' again to start a fresh session, then retry this tool call. If this is your very first connection to Patchbay, the user may need to approve access via a dialog on their screen before tools become available — after reinitializing, wait a few seconds for that approval, then retry."
    };
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": true
    })
}

/// Build the `tools/list` result (D2) for a dead/missing session: a SINGLE
/// synthetic tool whose name/description is exactly the kind of thing a model
/// reads to decide what it can do next — unlike a raw transport error, this
/// reaches the model even through a client that never surfaces HTTP-level
/// failures. No real jacks or meta tools are listed (we don't know the
/// client's identity/permissions without a session). `never_initialized`
/// mirrors [`dead_session_calltool_result`].
fn dead_session_toolslist_result(never_initialized: bool) -> serde_json::Value {
    let description = if never_initialized {
        "This is a placeholder — Patchbay has no active MCP session for you yet. Call 'initialize' first, then call tools/list again to see the real tools. If this is your very first connection, the user may need to approve access via a dialog on their screen — wait a few seconds after initializing, then retry."
    } else {
        "This is a placeholder — your MCP session with Patchbay is no longer valid (it expired, or Patchbay restarted). Call 'initialize' again to start a fresh session, then call tools/list again to see the real tools."
    };
    json!({
        "tools": [{
            "name": "patchbay__session_expired",
            "description": description,
            "inputSchema": { "type": "object", "properties": {} }
        }]
    })
}

// ---- GET /mcp (SSE: canonical per-session notification stream, D3) --------

pub async fn get_mcp(State(state): State<AppState>, headers: HeaderMap) -> Response {
    // 1. A valid Mcp-Session-Id resolves to a live session. Same 400-vs-404
    //    split as POST /mcp: a supplied-but-unknown session id (expired, or
    //    the gateway restarted) is 404 so the client knows to reinitialize;
    //    a missing header entirely is 400 (malformed request).
    let header_val = headers.get(MCP_SESSION_ID).and_then(|v| v.to_str().ok());
    let session = match header_val.and_then(|id| state.sessions.get(id)) {
        Some(s) => s,
        None if header_val.is_some() => {
            log("http: GET /mcp -> 404 (session not found/expired, client should reinitialize)");
            return (
                StatusCode::NOT_FOUND,
                "session not found or expired — call 'initialize' again to start a fresh session (Patchbay likely restarted)",
            )
                .into_response();
        }
        None => {
            log("http: GET /mcp -> 400 (missing Mcp-Session-Id)");
            return (StatusCode::BAD_REQUEST, "missing Mcp-Session-Id").into_response();
        }
    };

    // 2. Optional Last-Event-ID (reconnect replay).
    let last_event_id = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    // 3. Bounded channel feeding the SSE response (cap ~64, D3).
    let (tx, rx) = tokio::sync::mpsc::channel::<SseRecord>(64);

    // Keep our own sender only long enough to pre-seed the replay backlog, then
    // drop it so the channel closes (and the stream ends) when the canonical
    // slot's sender is later replaced/cleared.
    let replay_tx = tx.clone();

    // 4. Register as the canonical stream, REPLACING (closing) any prior one.
    let (stream_id, generation) = session.register_stream(tx);

    // 5. Drain the replay backlog into the channel ahead of live frames.
    let replay = session.take_replay_since(last_event_id.as_deref());
    for rec in replay {
        let _ = replay_tx.try_send(rec);
    }
    drop(replay_tx);

    log(&format!(
        "http: GET /mcp -> 200 SSE (session {} stream {} gen {})",
        session.id, stream_id, generation
    ));

    // 6. Map each record to an SSE Event; wrap in GuardedStream so a client
    //    disconnect generation-guarded unregisters this stream (D3).
    let stream = ReceiverStream::new(rx).map(|rec| sse::event_from_record(&rec));
    let guarded = sse::GuardedStream::new(stream, session, stream_id, generation);

    Sse::new(guarded)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(20)))
        .into_response()
}

// ---- POST /debug/toggle (debug-only test hook) ---------------------------

// Debug-only test hook (S5): drives the SAME `AppState::set_patched` pipeline
// as the tray so an automated curl can reproduce a toggle end-to-end (flip +
// persist + broadcast + start/stop + status). Compiled out of release builds
// so the shipped binary has no debug surface; the route registration in
// `build_router` is gated the same way.
#[cfg(debug_assertions)]
#[derive(serde::Deserialize)]
pub struct DebugToggleBody {
    jack: String,
    patched: bool,
}

#[cfg(debug_assertions)]
pub async fn debug_toggle(
    State(state): State<AppState>,
    Json(body): Json<DebugToggleBody>,
) -> Response {
    let result = state.set_patched(&body.jack, body.patched).await;
    log(&format!(
        "http: POST /debug/toggle jack={} patched={} -> status={}",
        body.jack, result.patched, result.status
    ));
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "jack": body.jack,
            "patched": result.patched,
            "status": result.status,
        })),
    )
        .into_response()
}

// ---- Admin jack management (S8) ------------------------------------------
//
// Secondary interface for non-MCP terminal/script agents: typed REST on the
// SAME axum router as /mcp (so it shares the `origin_guard` middleware and the
// strict 127.0.0.1 bind). These run in RELEASE builds too — NOT gated behind
// `#[cfg(debug_assertions)]` (unlike /debug/toggle) so scripts can manage jacks
// in a shipped binary. Both this interface and the meta MCP tools call the SAME
// `AppState::add_jack` / `remove_jack` / `list_jacks` / `set_patched` so they
// stay consistent.

/// `POST /admin/jacks` — add a jack. Body = `JackConfigInput` JSON.
/// 201 + `JackSummary` JSON on success; 400 + `{"error": ...}` on validation
/// failure (duplicate name, invalid name, missing required transport field).
/// (A malformed body or missing `transport` tag fails axum's `Json` extractor
/// with a 400 before reaching here.)
pub async fn admin_add_jack(
    State(state): State<AppState>,
    Json(input): Json<crate::config::JackConfigInput>,
) -> Response {
    match state.add_jack(input).await {
        Ok(summary) => (StatusCode::CREATED, Json(summary)).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.message() })),
        )
            .into_response(),
    }
}

/// `GET /admin/jacks` — list every jack as a JSON array of `JackSummary`.
pub async fn admin_list_jacks(State(state): State<AppState>) -> Response {
    Json(state.list_jacks()).into_response()
}

/// `DELETE /admin/jacks/{name}` — remove a jack.
/// 200 on success; 404 if the jack does not exist; 500 on a persist failure.
pub async fn admin_remove_jack(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Response {
    match state.remove_jack(&name).await {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({ "removed": name }))).into_response(),
        Err(crate::app_state::RemoveJackError::NotFound(_)) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("jack '{}' not found", name) })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.message() })),
        )
            .into_response(),
    }
}

/// Body for `POST /admin/jacks/{name}/toggle`.
#[derive(serde::Deserialize)]
pub struct AdminToggleBody {
    pub patched: bool,
}

/// `POST /admin/jacks/{name}/toggle` — turn a jack ON/OFF. Body `{"patched":
/// bool}`. Routes through the EXISTING `AppState::set_patched` (same pipeline as
/// the tray toggle). 200 + `{name,patched,status}` on success; 404 if the jack
/// does not exist.
pub async fn admin_toggle_jack(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<AdminToggleBody>,
) -> Response {
    // Existence check for a clean 404 (set_patched itself tolerates unknown
    // names without erroring).
    let exists = {
        let cfg = state.config.read();
        cfg.jacks.iter().any(|j| j.name == name)
    };
    if !exists {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("jack '{}' not found", name) })),
        )
            .into_response();
    }
    let result = state.set_patched(&name, body.patched).await;
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "name": name,
            "patched": result.patched,
            "status": result.status,
        })),
    )
        .into_response()
}

// ---- DELETE /mcp ---------------------------------------------------------

pub async fn delete_mcp(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let session_id = headers.get(MCP_SESSION_ID).and_then(|v| v.to_str().ok());
    match session_id {
        Some(id) => {
            state.sessions.remove(id);
            log(&format!("http: DELETE session {} -> 204", id));
            StatusCode::NO_CONTENT.into_response()
        }
        None => {
            log("http: DELETE -> 400 (missing Mcp-Session-Id)");
            StatusCode::BAD_REQUEST.into_response()
        }
    }
}

// ---- response helpers ----------------------------------------------------

/// Turn a [`DispatchOutcome`] into an axum `Response` (status + body + the
/// `Mcp-Session-Id` header when present).
fn build_response(outcome: &DispatchOutcome) -> Response {
    let status = StatusCode::from_u16(outcome.http_status).unwrap_or(StatusCode::OK);
    status_json(status, outcome.body.as_ref(), outcome.new_session_id.as_deref())
}

/// Build a JSON (or empty) response with an optional `Mcp-Session-Id` header.
fn status_json(
    status: StatusCode,
    body: Option<&serde_json::Value>,
    session_id: Option<&str>,
) -> Response {
    let mut response = match body {
        Some(v) => (status, Json(v.clone())).into_response(),
        None => (status, "").into_response(),
    };
    if let Some(id) = session_id {
        if let Ok(val) = HeaderValue::from_str(id) {
            response
                .headers_mut()
                .insert(HeaderName::from_static(MCP_SESSION_ID), val);
        }
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn localhost_origins_are_allowed() {
        assert!(is_localhost_origin("http://127.0.0.1:39100"));
        assert!(is_localhost_origin("http://localhost:39100"));
        assert!(is_localhost_origin("http://localhost"));
        assert!(is_localhost_origin("http://[::1]:8080"));
        assert!(is_localhost_origin("http://127.0.0.1:39100/some/path"));
    }

    #[test]
    fn non_localhost_origins_are_rejected() {
        assert!(!is_localhost_origin("https://example.com"));
        assert!(!is_localhost_origin("http://10.0.0.1:39100"));
        assert!(!is_localhost_origin("https://evil.com:443/x"));
    }

    #[test]
    fn dead_session_calltool_result_is_a_model_readable_error() {
        for never_initialized in [true, false] {
            let v = dead_session_calltool_result(never_initialized);
            assert_eq!(v["isError"], true);
            let text = v["content"][0]["text"].as_str().unwrap();
            assert_eq!(v["content"][0]["type"], "text");
            assert!(text.contains("initialize"), "must tell the model to call initialize, got: {text}");
        }
    }

    #[test]
    fn dead_session_calltool_result_wording_distinguishes_never_vs_expired() {
        let never = dead_session_calltool_result(true);
        let expired = dead_session_calltool_result(false);
        let never_text = never["content"][0]["text"].as_str().unwrap();
        let expired_text = expired["content"][0]["text"].as_str().unwrap();
        assert!(never_text.contains("have not established"));
        assert!(expired_text.contains("no longer valid"));
    }

    #[test]
    fn dead_session_toolslist_result_exposes_one_synthetic_tool() {
        for never_initialized in [true, false] {
            let v = dead_session_toolslist_result(never_initialized);
            let tools = v["tools"].as_array().unwrap();
            assert_eq!(tools.len(), 1, "no real jacks/meta tools without a resolved session");
            assert_eq!(tools[0]["name"], "patchbay__session_expired");
            let desc = tools[0]["description"].as_str().unwrap();
            assert!(desc.contains("initialize"), "must tell the model to call initialize, got: {desc}");
            assert_eq!(tools[0]["inputSchema"]["type"], "object");
        }
    }
}
