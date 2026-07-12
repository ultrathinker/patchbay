//! MCP method dispatch for a single POSTed JSON-RPC message.
//!
//! The HTTP layer (`http.rs`) is responsible for session-id enforcement and
//! HTTP status wiring; this module only decides *what* the MCP result is and
//! returns a [`DispatchOutcome`] the HTTP layer turns into (status, optional
//! `Mcp-Session-Id` header, body).
//!
//! Stage 2 implements only the lifecycle: `initialize`,
//! `notifications/initialized`, and an empty `tools/list`. Upstream tools
//! (S4/S5), `tools/call` (S5) and the broadcast (S3) come later.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use serde_json::{json, Value};

use crate::app_state::AppState;
use crate::gateway::jsonrpc;
use crate::gateway::session::{ClientSession, SessionId};
use crate::gateway::tools;

/// Protocol version Patchbay prefers and advertises by default (MASTER_PLAN D4).
pub const PREFERRED_PROTOCOL_VERSION: &str = "2025-06-18";

/// Protocol versions we will echo back if the client requests one of them.
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26"];

// ---- S8: gateway-owned meta tools (add/remove/list/toggle jack) -----------
//
// Four built-in tools the gateway answers itself — they are NEVER routed to an
// upstream jack. Their names happen to contain `__` (so they read as
// `patchbay__<verb>`), which is exactly why `tools/call` must intercept them by
// EXACT name BEFORE the `<jack>__<tool>` namespace split: without that, the
// split would treat them as tools of a jack named "patchbay" and return
// "unknown tool". They always appear in `tools/list` regardless of which jacks
// are patched/unpatched (gateway-owned, not upstream-owned).
pub const BUILTIN_ADD_JACK: &str = "patchbay__add_jack";
pub const BUILTIN_REMOVE_JACK: &str = "patchbay__remove_jack";
pub const BUILTIN_LIST_JACKS: &str = "patchbay__list_jacks";
pub const BUILTIN_TOGGLE_JACK: &str = "patchbay__toggle_jack";

/// The result of dispatching one inbound JSON-RPC message. The HTTP layer maps
/// `http_status` to an HTTP status, sets `new_session_id` on the
/// `Mcp-Session-Id` header when present, and serializes `body` (empty body when
/// `None`, e.g. notifications).
#[derive(Debug)]
pub struct DispatchOutcome {
    pub http_status: u16,
    pub new_session_id: Option<SessionId>,
    pub body: Option<Value>,
}

impl DispatchOutcome {
    /// 200 OK with a JSON-RPC body.
    pub fn ok(body: Value) -> Self {
        DispatchOutcome {
            http_status: 200,
            new_session_id: None,
            body: Some(body),
        }
    }

    /// 200 OK with a JSON-RPC body, plus a freshly minted `Mcp-Session-Id`.
    pub fn ok_with_session(session_id: SessionId, body: Value) -> Self {
        DispatchOutcome {
            http_status: 200,
            new_session_id: Some(session_id),
            body: Some(body),
        }
    }

    /// 202 Accepted with an empty body (notification ack).
    pub fn accepted() -> Self {
        DispatchOutcome {
            http_status: 202,
            new_session_id: None,
            body: None,
        }
    }
}

/// Dispatch one inbound JSON-RPC request.
///
/// `session` is `None` for `initialize` (the call that *creates* a session) and
/// `Some(resolved)` for every other method — the HTTP layer guarantees a valid
/// `Mcp-Session-Id` was supplied before reaching here for non-initialize calls.
///
/// `header_client` is the (already trimmed, non-empty) value of the
/// `X-Patchbay-Client` HTTP header when present (S10b). It is consulted ONLY on
/// `initialize`, where it takes PRIORITY over `clientInfo.name` for resolving
/// the session's client identity; every other method ignores it (the resolved
/// identity is cached on the session at initialize time).
///
/// `async` because `tools/call` is forwarded to an upstream child (S4); the
/// lifecycle paths (`initialize`, `initialized`) and the merged `tools/list`
/// (a synchronous cache snapshot) do not actually await.
pub async fn dispatch(
    state: &AppState,
    req: &jsonrpc::JsonRpcRequest,
    session: Option<Arc<ClientSession>>,
    header_client: Option<&str>,
) -> DispatchOutcome {
    // (FIX 9) Any inbound request keeps the session alive (idle reaping).
    if let Some(s) = &session {
        s.touch();
    }
    match req.method.as_str() {
        "initialize" => handle_initialize(state, req, header_client).await,
        "notifications/initialized" => handle_initialized(session),
        "tools/list" => handle_tools_list(state, req, session.clone()),
        "tools/call" => handle_tools_call(state, req, session.clone()).await,
        "ping" => {
            // Empty-result liveness probe (MCP `ping`).
            let body = serde_json::to_value(jsonrpc::success(req.id.clone(), json!({})))
                .expect("ping response is always serializable");
            DispatchOutcome::ok(body)
        }
        other => {
            // Unknown method. Requests (id present) get a JSON-RPC -32601;
            // unknown notifications are silently acked at 202.
            if req.id.is_none() {
                DispatchOutcome::accepted()
            } else {
                let body = serde_json::to_value(jsonrpc::error(
                    req.id.clone(),
                    jsonrpc::METHOD_NOT_FOUND,
                    format!("method not found: {}", other),
                    None,
                ))
                .expect("jsonrpc error is always serializable");
                DispatchOutcome::ok(body)
            }
        }
    }
}

/// `initialize`: create a session, negotiate a protocol version, advertise
/// capabilities (`tools.listChanged` so clients honor our later broadcasts),
/// and return server info + a short instructions string. The HTTP layer puts
/// the new session id into the `Mcp-Session-Id` response header.
///
/// (S10) `clientInfo.name` (and `.version` for display) is extracted from the
/// initialize params and stored on the session; the name is also recorded in
/// `seen_clients` on first sight so the tray "Custom" submenu can list the
/// agent. A missing/malformed `clientInfo` is tolerated: the name stays `None`
/// and the client falls back to the global list only (no crash).
///
/// (S10b) `header_client` — the `X-Patchbay-Client` HTTP header value (already
/// trimmed + empty-filtered by the HTTP layer, re-trimmed here for defense) —
/// takes PRIORITY over `clientInfo.name`. This lets the USER choose a stable,
/// human-chosen identity from the agent's connection config (e.g. distinguishing
/// two windows of the same agent), instead of relying on whatever string the
/// agent's code happens to self-report. Absent/empty header -> graceful fallback
/// to `clientInfo.name`; both absent -> `None` (unchanged S10 behavior).
///
/// (S10c) `async` because the first-connection approval gate
/// ([`AppState::ensure_client_approved`]) may block the request on a native
/// Win32 dialog while the user decides Allow/Deny for a never-seen identity.
/// Only this one request's task is suspended; other clients are unaffected. On
/// Allow (or gate-OFF) the client is recorded in `seen_clients`; on Deny the
/// identity lands in `forbidden_clients` and the `seen_clients` record is
/// skipped — the agent gets zero tools, but `initialize` still completes (no
/// transport-level failure).
async fn handle_initialize(
    state: &AppState,
    req: &jsonrpc::JsonRpcRequest,
    header_client: Option<&str>,
) -> DispatchOutcome {
    // Prefer 2025-06-18; echo the client's version if it named a supported one.
    let client_pv = req
        .params
        .as_ref()
        .and_then(|p| p.get("protocolVersion"))
        .and_then(|v| v.as_str());
    let chosen: &str = match client_pv {
        Some(v) if SUPPORTED_PROTOCOL_VERSIONS.contains(&v) => v,
        _ => PREFERRED_PROTOCOL_VERSION,
    };

    // (S10) Extract the connecting client's self-reported identity from
    // `clientInfo`. The version always comes from here (the header carries only
    // a name/label, never a version).
    let (client_info_name, client_version): (Option<String>, Option<String>) = req
        .params
        .as_ref()
        .and_then(|p| p.get("clientInfo"))
        .and_then(|ci| {
            let name = ci.get("name")?.as_str()?.to_string();
            let version = ci.get("version").and_then(|v| v.as_str()).map(str::to_owned);
            Some((Some(name), version))
        })
        .unwrap_or((None, None));

    // (S10b) Resolve the client identity with HEADER PRIORITY: the user-chosen
    // `X-Patchbay-Client` header wins over `clientInfo.name`. The HTTP layer
    // already trimmed + empty-filtered; we re-trim/filter here so the function
    // stays correct for any caller (e.g. tests). Both absent -> None (S10).
    let client_name = header_client
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or(client_info_name);

    // (Review fix) An UNIDENTIFIED connection (no `X-Patchbay-Client` header AND
    // no `clientInfo.name`) can never be looked up in `forbidden_clients` or
    // `client_overrides` — both are keyed by name, and `is_forbidden(None)` is
    // unconditionally `false` (see `config::schema`). With the approval gate ON,
    // routing such a client through `ensure_client_approved` would be pointless
    // (there is no stable name to record a Deny against — the very next
    // anonymous connection would trip the dialog again, or worse, silently keep
    // getting the same default-global access the gate exists to block). So while
    // the gate is ON, refuse to establish a session at all for an unidentified
    // client, with a message telling the agent what to send instead. This does
    // NOT affect a normal client that sends `clientInfo.name` (nearly all
    // compliant MCP clients do) — only a hand-crafted request omitting both.
    // Gate OFF is unaffected: unidentified clients still get the pre-existing
    // fall-back-to-global behavior (tested by
    // `initialize_no_header_no_client_info_is_none`).
    if client_name.is_none() && state.config.read().require_approval_for_new_clients {
        let resp = jsonrpc::error(
            req.id.clone(),
            jsonrpc::SERVER_ERROR,
            "Patchbay requires new agents to identify themselves (this server has \
             'Require approval for new agents' enabled). Send an MCP clientInfo.name \
             in your initialize request, or set the X-Patchbay-Client HTTP header.",
            None,
        );
        let body = serde_json::to_value(resp).expect("jsonrpc error is always serializable");
        return DispatchOutcome::ok(body);
    }

    let session = state.sessions.create();
    *session.protocol_version.write() = chosen.to_string();
    session.set_client_name(client_name.clone());

    // (S10c) First-connection approval gate. For a NEW identity with the gate ON
    // this blocks on a native Win32 dialog until the user answers (the request
    // stays pending for just this one async task). On Allow the identity is
    // recorded in seen_clients (with the agent's version) atomically inside the
    // gate; on Deny it lands in forbidden_clients and is NOT recorded — the
    // agent gets zero tools, but initialize still completes. A gate-OFF/known/
    // forbidden identity returns immediately (gate-OFF auto-records the client —
    // today's S10 behavior). The resolved identity (header priority, S10b)
    // drives the record. (An unidentified client with the gate ON never reaches
    // here — see above.)
    if let Some(name) = &client_name {
        state
            .ensure_client_approved(name, client_version.as_deref())
            .await;
    }

    let result = json!({
        "protocolVersion": chosen,
        "capabilities": {
            "tools": { "listChanged": true }
        },
        "serverInfo": {
            "name": "patchbay",
            "title": "Patchbay",
            "version": env!("CARGO_PKG_VERSION")
        },
        "instructions": "Patchbay exposes upstream MCP servers as namespaced tools named <jack>__<tool>. If a tool call returns an \"unpatched\" error, the user has disabled that server in the Patchbay tray; ask them to re-enable it there."
    });

    let body = serde_json::to_value(jsonrpc::success(req.id.clone(), result))
        .expect("initialize response is always serializable");
    DispatchOutcome::ok_with_session(session.id.clone(), body)
}

/// `notifications/initialized`: mark the session initialized. MCP defines no
/// JSON-RPC response to a notification, so the HTTP layer returns 202 + empty.
///
/// A `tools/list_changed` that should reach this session is handled by the S3
/// broadcast path: if a change occurred between `initialize` and `initialized`,
/// the session was not yet initialized at broadcast time, so `dirty_tools_list`
/// was set; it is coalesced into one `list_changed` on the session's next
/// `GET /mcp` (see `ClientSession::take_replay_since`).
fn handle_initialized(session: Option<Arc<ClientSession>>) -> DispatchOutcome {
    if let Some(s) = session {
        s.initialized.store(true, Ordering::Release);
    }
    DispatchOutcome::accepted()
}

/// `tools/list`: return the MERGED, NAMESPACED tools from every Running upstream
/// jack (S4), FILTERED by the per-client effective `patched` flag (S5 + S10).
/// Each upstream tool is renamed `<jack>__<tool>`; all other fields (description,
/// inputSchema, …) are passed through verbatim.
///
/// The patched filter is what makes the OFF toggle's "broadcast then stop"
/// ordering correct: the instant a jack flips to `patched == false` it vanishes
/// from `tools/list` (and `tools/call` returns the UNPATCHED taxonomy) even
/// though its child is killed a moment later — so the broadcast sent before the
/// kill already reflects the new world.
///
/// (S10) The filter now uses `effective_patched(jack, session's client_name)`
/// instead of the raw global flag, so a Custom client sees only ITS list while a
/// global-default client sees the global list. The session's `client_name` is
/// snapshotted under a cheap read (dropped before the config lock) — never two
/// locks held at once. The gateway-owned meta tools are ALWAYS present and are
/// NOT subject to per-client filtering.
fn handle_tools_list(
    state: &AppState,
    req: &jsonrpc::JsonRpcRequest,
    session: Option<Arc<ClientSession>>,
) -> DispatchOutcome {
    // Snapshot the connecting client's name (cheap read, dropped before the
    // config lock — match the existing snapshot-then-drop discipline).
    let client_name: Option<String> = session
        .as_ref()
        .and_then(|s| s.client_name.read().clone());
    // Build the per-client effective patched set under one config read lock.
    // (S10c) A forbidden client's effective_patched is false for every jack, so
    // effective_names is empty for it — and we additionally withhold the
    // gateway-owned meta tools below ("a forbidden agent should see nothing").
    let (forbidden, effective_names): (bool, std::collections::HashSet<String>) = {
        let cfg = state.config.read();
        let forbidden = cfg.is_forbidden(client_name.as_deref());
        let names = cfg
            .jacks
            .iter()
            .filter(|j| cfg.effective_patched(&j.name, client_name.as_deref()))
            .map(|j| j.name.clone())
            .collect();
        (forbidden, names)
    };
    let mut tools_out = Vec::new();
    for (jack, tool) in state.upstream.cached_tools() {
        // Only jacks effective-patched for THIS client contribute (a briefly-
        // still-alive child the client shouldn't see is hidden the moment its
        // effective flag flips off).
        if effective_names.contains(&jack) {
            tools_out.push(tools::namespaced_tool(&jack, &tool));
        }
    }
    // S8: gateway-owned meta tools are ALWAYS present for an ALLOWED client,
    // regardless of which jacks are patched/unpatched or who the client is. An
    // agent connected via MCP can manage Patchbay's own configuration through
    // these without leaving the session.
    //
    // (S10c) A forbidden client sees NOTHING — no jack tools (effective_patched
    // already hid them) and no meta tools either.
    if !forbidden {
        for tool in builtin_admin_tools() {
            tools_out.push(tool);
        }
    }
    let result = json!({ "tools": tools_out });
    let body = serde_json::to_value(jsonrpc::success(req.id.clone(), result))
        .expect("tools/list response is always serializable");
    DispatchOutcome::ok(body)
}

/// `tools/call`: the D2 enforcement taxonomy (MASTER_PLAN D2).
///
/// 0. (S10c) Forbidden client -> `CallToolResult{isError:true}` with the "blocked
///    by the user" text, BEFORE the meta-tool interception and the namespace
///    split (so a blocked agent sees no tool surface at all).
/// 1. Split `<jack>__<tool>` on the first `__`; no `__`, or a jack name not in
///    the config, -> JSON-RPC `invalid_params` "unknown tool '<name>'"
///    (genuinely malformed/unknown -> a protocol-level error is fine).
/// 2. Known jack but effectively UNPATCHED FOR THIS CLIENT -> a `CallToolResult`
///    with `isError: true` carrying the UNPATCHED model-directed text. The text
///    distinguishes "disabled for THIS agent by a per-agent custom setting" from
///    the global default so the model gets an accurate story. Returned as a
///    normal JSON-RPC `result` object, NOT a protocol error.
/// 3. Patched for this client but not Running (Starting/Failed) ->
///    `CallToolResult{isError:true}` naming the jack + the last error.
/// 4. Running -> route to the upstream child; success passes through; a
///    timeout/transport error -> `CallToolResult{isError:true}` naming the jack.
///
/// (S10) The effective-patched check uses `effective_patched(jack, session's
/// client_name)`. The shared child may be alive (some OTHER client needs it)
/// while THIS client is denied — process lifecycle vs per-client visibility are
/// kept separate. Gateway-owned meta tools are intercepted BEFORE the namespace
/// split and are never subject to this filter.
async fn handle_tools_call(
    state: &AppState,
    req: &jsonrpc::JsonRpcRequest,
    session: Option<Arc<ClientSession>>,
) -> DispatchOutcome {
    let params = req.params.clone().unwrap_or(Value::Null);
    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);

    // Snapshot the connecting client's name (cheap read, dropped before the
    // config lock). Taken up front so the forbidden gate below can use it.
    let client_name: Option<String> = session
        .as_ref()
        .and_then(|s| s.client_name.read().clone());

    // (S10c) Forbidden-client gate — BEFORE the meta-tool interception and the
    // namespace split, so a blocked agent's EVERY tools/call (including a meta
    // tool) returns the D2-style CallToolResult{is_error:true} with a "blocked
    // by the user" message. Reuses the SAME response shape as the unpatched-jack
    // enforcement (a JSON-RPC result with isError, NOT a protocol error).
    {
        let cfg = state.config.read();
        if cfg.is_forbidden(client_name.as_deref()) {
            return call_tool_result(
                req,
                call_tool_error_result(&forbidden_text(client_name.as_deref())),
            );
        }
    }

    // (S8) Gateway-owned meta tools: intercept by EXACT name BEFORE the
    // `<jack>__<tool>` namespace split. These names contain `__` but belong to
    // no upstream jack — they must never be forwarded upstream. (There is no
    // real jack named "patchbay"; even if a user later added one, only these
    // four exact names are intercepted, so a real `patchbay__<other>` tool would
    // still route to that jack normally.)
    match name {
        BUILTIN_ADD_JACK => return call_meta_add_jack(state, req, arguments).await,
        BUILTIN_REMOVE_JACK => return call_meta_remove_jack(state, req, arguments).await,
        BUILTIN_LIST_JACKS => return call_meta_list_jacks(state, req).await,
        BUILTIN_TOGGLE_JACK => return call_meta_toggle_jack(state, req, arguments).await,
        _ => {}
    }

    let (jack, tool) = match tools::split_namespaced(name) {
        Some((j, t)) => (j.to_string(), t.to_string()),
        None => return tool_unknown(req, name),
    };

    // Look up the jack + resolve effective patched FOR THIS CLIENT under one
    // config read (so an unpatch/override just toggled is enforced immediately,
    // with no restart). Also note whether the block came from a per-client
    // override (for an accurate model-directed message).
    let (patched, client_specific_block) = {
        let cfg = state.config.read();
        if !cfg.jacks.iter().any(|j| j.name == jack) {
            return tool_unknown(req, name);
        }
        let eff = cfg.effective_patched(&jack, client_name.as_deref());
        let client_specific = !eff
            && client_name
                .as_deref()
                .and_then(|n| cfg.client_overrides.get(n))
                .map(|o| o.enabled && o.jacks.get(&jack) == Some(&false))
                .unwrap_or(false);
        (eff, client_specific)
    };

    // (2) Known but effectively UNPATCHED for this client -> model-directed
    // CallToolResult error (wording reflects whether a per-agent override is the
    // cause, vs. the global default).
    if !patched {
        let text = unpatched_text(&jack, client_name.as_deref(), client_specific_block);
        return call_tool_result(req, call_tool_error_result(&text));
    }

    // (3) Patched for this client but not Running (Starting/Failed).
    if !state.upstream.is_jack_running(&jack) {
        let reason = not_running_reason(state.upstream.status_string(&jack));
        let text = format!(
            "Patchbay: server '{}' is patched but not running (last error: {}). Toggling it off and on in the tray restarts it.",
            jack, reason
        );
        return call_tool_result(req, call_tool_error_result(&text));
    }

    // (4) Running -> route to the upstream child.
    match state.upstream.route_call(&jack, &tool, arguments).await {
        Ok(result) => {
            let body = serde_json::to_value(jsonrpc::success(req.id.clone(), result))
                .expect("tools/call success is always serializable");
            DispatchOutcome::ok(body)
        }
        Err(e) => {
            let text = format!("Patchbay: call to server '{}' failed ({}).", jack, e);
            call_tool_result(req, call_tool_error_result(&text))
        }
    }
}

/// The UNPATCHED model-directed text (MASTER_PLAN D2 wording verbatim), with a
/// per-client variant (S10) when a per-agent custom setting is what's blocking
/// THIS client — so the model gets an accurate story and asks the user to fix
/// the right list (the Custom submenu, not the global toggle).
fn unpatched_text(jack: &str, client_name: Option<&str>, client_specific: bool) -> String {
    if client_specific {
        let name = client_name.unwrap_or("this agent");
        format!(
            "Patchbay: server '{}' is disabled for THIS agent ('{}') by a Patchbay per-agent custom setting; other agents may still have it enabled. Do not retry; ask the user to re-enable '{}' for '{}' in the Patchbay tray (Custom submenu) if this tool is required.",
            jack, name, jack, name
        )
    } else {
        format!(
            "Patchbay: server '{}' is currently UNPATCHED (disabled) in the Patchbay tray. All of its tools are unavailable to every agent until the user re-enables it. Do not retry; ask the user to enable '{}' in Patchbay if this tool is required.",
            jack, jack
        )
    }
}

/// The BLOCKED-by-user model-directed text (S10c). A forbidden client's every
/// `tools/call` (and the empty `tools/list`) carries this; it reuses the SAME
/// `CallToolResult{is_error:true}` shape as [`unpatched_text`] so the model gets
/// model-directed text (stops retrying, asks the user) rather than a protocol
/// error. Names the identity so the user knows which entry to remove in the tray.
fn forbidden_text(client_name: Option<&str>) -> String {
    let name = client_name.unwrap_or("This agent");
    format!(
        "Patchbay: this agent ('{}') has been blocked by the user from this Patchbay instance. \
         No MCP servers or tools are available to it. Do not retry; ask the user to remove '{}' \
         from the Patchbay tray (Settings → Forbidden) if access should be granted.",
        name, name
    )
}

/// Map an upstream status string (`failed: <r>` / `starting` / …) onto the
/// "last error" phrase used in the not-running CallToolResult text.
fn not_running_reason(status: String) -> String {
    match status.as_str() {
        "starting" => "starting".to_string(),
        s if s.starts_with("failed:") => s["failed:".len()..].trim().to_string(),
        _ => "not running".to_string(),
    }
}

/// Build an MCP `CallToolResult` error value: a JSON-RPC `result` object (NOT a
/// JSON-RPC error) shaped `{ content:[{type:text,text}], isError:true }`. The
/// text is delivered verbatim into model context by every MCP client, which is
/// the whole point of the D2 taxonomy (model stops retrying, asks the user).
fn call_tool_error_result(text: &str) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": true
    })
}

/// Build a non-error MCP `CallToolResult` (S8 meta-tool success): the same
/// shape as [`call_tool_error_result`] but with `isError: false`, so the gateway
/// owns a clean text result the same way it owns the D2 error text.
fn ok_text_result(text: &str) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": false
    })
}

/// Wrap a `CallToolResult` value into a JSON-RPC success response (200).
fn call_tool_result(req: &jsonrpc::JsonRpcRequest, result: Value) -> DispatchOutcome {
    let body = serde_json::to_value(jsonrpc::success(req.id.clone(), result))
        .expect("call tool result is always serializable");
    DispatchOutcome::ok(body)
}

/// JSON-RPC `invalid_params` "unknown tool '<name>'" for a malformed/unknown
/// tool name (no `__`, or jack not in config) — the D2 protocol-level case.
fn tool_unknown(req: &jsonrpc::JsonRpcRequest, name: &str) -> DispatchOutcome {
    let body = serde_json::to_value(jsonrpc::error(
        req.id.clone(),
        jsonrpc::INVALID_PARAMS,
        format!("unknown tool '{}'", name),
        None,
    ))
    .expect("jsonrpc error is always serializable");
    DispatchOutcome::ok(body)
}

// ---- S8: gateway-owned meta-tool definitions + dispatch -------------------

/// The four gateway-owned meta tools always present in `tools/list`. The
/// `inputSchema` for `add_jack` mirrors [`JackConfigInput`] exactly (same
/// discriminator `transport` with kebab-case tags `stdio`/`streamable-http`,
/// same variant fields), so what an agent sends round-trips through
/// `patchbay.json` identically to a hand-written jack.
fn builtin_admin_tools() -> Vec<Value> {
    vec![
        json!({
            "name": BUILTIN_ADD_JACK,
            "description": "Add a new MCP server (\"jack\") to Patchbay. Its tools become available to every agent as namespaced tools <jack>__<tool>. Provide a unique 'name', a 'transport' ('stdio' with a 'command', or 'streamable-http' with a 'url'), and that transport's fields. The jack is saved to patchbay.json and, when patched (the default), started immediately. Returns the new jack's runtime status and tool count.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Unique jack name: [A-Za-z0-9_-]+, no '__', <= 40 chars. Used as the tool namespace prefix <name>__<tool>." },
                    "patched": { "type": "boolean", "default": true, "description": "true (default) starts the jack immediately; false adds it disabled (no tools until toggled on)." },
                    "transport": { "type": "string", "enum": ["stdio", "streamable-http"], "description": "Transport discriminator. 'stdio': spawn a local process (needs 'command'). 'streamable-http': connect to a remote MCP server (needs 'url')." },
                    "command": { "type": "string", "description": "(stdio) Executable to run, e.g. 'npx'. Required when transport is 'stdio'." },
                    "args": { "type": "array", "items": { "type": "string" }, "description": "(stdio) Arguments passed to 'command'." },
                    "env": { "type": "object", "description": "(stdio) Environment variables. Values may be plaintext; Patchbay DPAPI-encrypts them on save." },
                    "url": { "type": "string", "description": "(streamable-http) MCP server URL. Required when transport is 'streamable-http'." },
                    "headers": { "type": "object", "description": "(streamable-http) Request headers, e.g. Authorization. Values may be plaintext; encrypted on save." },
                    "sharing": { "type": "string", "enum": ["shared", "per_client_session"], "default": "shared", "description": "Sharing model; v0.1 implements 'shared'." }
                },
                "required": ["name", "transport"]
            }
        }),
        json!({
            "name": BUILTIN_REMOVE_JACK,
            "description": "Remove a jack from Patchbay: stops its upstream client if running, deletes it from patchbay.json, and broadcasts the change. Other agents will stop seeing its tools.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Name of the jack to remove." }
                },
                "required": ["name"]
            }
        }),
        json!({
            "name": BUILTIN_LIST_JACKS,
            "description": "List every jack configured in Patchbay with its patched state, transport type, runtime status, and tool count. Use this to read Patchbay's current state without opening the config file.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": BUILTIN_TOGGLE_JACK,
            "description": "Turn a jack ON (patched:true) or OFF (patched:false) in Patchbay. Takes effect immediately for every connected agent (a tools/list_changed broadcast is sent). This is the same pipeline as the tray checkbox toggle.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Name of the jack to toggle." },
                    "patched": { "type": "boolean", "description": "true = ON (start the upstream); false = OFF (stop it)." }
                },
                "required": ["name", "patched"]
            }
        }),
    ]
}

/// `patchbay__add_jack` dispatch: deserialize the arguments as [`JackConfigInput`]
/// and drive [`AppState::add_jack`]. Any failure (bad JSON, invalid name,
/// duplicate, missing required field) is a `CallToolResult{isError:true}` (D2
/// style), never a JSON-RPC protocol error, so the text lands in model context.
async fn call_meta_add_jack(
    state: &AppState,
    req: &jsonrpc::JsonRpcRequest,
    arguments: Value,
) -> DispatchOutcome {
    let input: crate::config::JackConfigInput = match serde_json::from_value(arguments) {
        Ok(i) => i,
        Err(e) => {
            return call_tool_result(
                req,
                call_tool_error_result(&format!(
                    "Patchbay: invalid add_jack arguments ({}). Provide 'name' (string), \
                     'transport' ('stdio' or 'streamable-http'), and that transport's required \
                     field ('command' for stdio, 'url' for streamable-http).",
                    e
                )),
            );
        }
    };
    match state.add_jack(input).await {
        Ok(s) => {
            let text = format!(
                "Patchbay: added jack '{}' ({}), status: {}, {} tools available.",
                s.name, s.transport, s.status, s.tool_count
            );
            call_tool_result(req, ok_text_result(&text))
        }
        Err(e) => call_tool_result(
            req,
            call_tool_error_result(&format!("Patchbay: could not add jack: {}", e.message())),
        ),
    }
}

/// `patchbay__remove_jack` dispatch.
async fn call_meta_remove_jack(
    state: &AppState,
    req: &jsonrpc::JsonRpcRequest,
    arguments: Value,
) -> DispatchOutcome {
    let name = match arguments.get("name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => {
            return call_tool_result(
                req,
                call_tool_error_result("Patchbay: remove_jack requires 'name' (string)."),
            );
        }
    };
    match state.remove_jack(&name).await {
        Ok(()) => call_tool_result(
            req,
            ok_text_result(&format!("Patchbay: removed jack '{}'.", name)),
        ),
        Err(e) => call_tool_result(
            req,
            call_tool_error_result(&format!("Patchbay: could not remove jack: {}", e.message())),
        ),
    }
}

/// `patchbay__list_jacks` dispatch. Formats the summaries as readable text so an
/// agent can parse current state without opening the config file.
async fn call_meta_list_jacks(state: &AppState, req: &jsonrpc::JsonRpcRequest) -> DispatchOutcome {
    let jacks = state.list_jacks();
    let text = if jacks.is_empty() {
        "Patchbay: no jacks configured.".to_string()
    } else {
        let mut lines = vec!["Patchbay jacks:".to_string()];
        for j in &jacks {
            lines.push(format!(
                "- {} ({}): patched={}, status={}, tools={}",
                j.name, j.transport, j.patched, j.status, j.tool_count
            ));
        }
        lines.join("\n")
    };
    call_tool_result(req, ok_text_result(&text))
}

/// `patchbay__toggle_jack` dispatch. Routes through the EXISTING
/// [`AppState::set_patched`] (no reimplementation). A not-found jack yields a
/// `CallToolResult{isError:true}` (set_patched itself tolerates unknown names
/// without erroring, so the existence is checked here for a clear message).
async fn call_meta_toggle_jack(
    state: &AppState,
    req: &jsonrpc::JsonRpcRequest,
    arguments: Value,
) -> DispatchOutcome {
    let name = arguments.get("name").and_then(|v| v.as_str());
    let patched = arguments.get("patched").and_then(|v| v.as_bool());
    let (name, patched) = match (name, patched) {
        (Some(n), Some(p)) => (n.to_string(), p),
        _ => {
            return call_tool_result(
                req,
                call_tool_error_result(
                    "Patchbay: toggle_jack requires 'name' (string) and 'patched' (boolean).",
                ),
            );
        }
    };
    // Existence check for a clear not-found error (set_patched returns no error
    // for an unknown jack — it just reports patched:false/status:unknown).
    let exists = {
        let cfg = state.config.read();
        cfg.jacks.iter().any(|j| j.name == name)
    };
    if !exists {
        return call_tool_result(
            req,
            call_tool_error_result(&format!("Patchbay: jack '{}' not found.", name)),
        );
    }
    let result = state.set_patched(&name, patched).await;
    let text = format!(
        "Patchbay: jack '{}' is now {} (status: {}).",
        name,
        if result.patched {
            "patched (on)"
        } else {
            "unpatched (off)"
        },
        result.status
    );
    call_tool_result(req, ok_text_result(&text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use crate::config::first_run_template;
    use serde_json::json;

    fn req(method: &str, id: Option<Value>) -> jsonrpc::JsonRpcRequest {
        jsonrpc::JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id,
            method: method.to_string(),
            params: None,
        }
    }

    fn state() -> AppState {
        // (S10c) The approval gate is ON by default, which would pop a real
        // Win32 MessageBoxW during these initialize-path tests. Turn it OFF for
        // the shared test state so identity resolution + seen_clients recording
        // (today's S10 behavior) is exercised without a dialog. Tests that
        // specifically cover the gate/forbidden enforcement build their own
        // config.
        let st = AppState::new(first_run_template());
        st.config.write().require_approval_for_new_clients = false;
        st
    }

    #[tokio::test]
    async fn initialize_creates_session_and_advertises_list_changed() {
        let st = state();
        let r = req("initialize", Some(json!(1)));
        let outcome = dispatch(&st, &r, None, None).await;
        assert_eq!(outcome.http_status, 200);
        let sid = outcome
            .new_session_id
            .clone()
            .expect("initialize must mint a session id");
        assert!(!sid.is_empty());

        let body = outcome.body.expect("initialize must have a body");
        assert_eq!(body["jsonrpc"], "2.0");
        assert_eq!(body["id"], 1);
        assert_eq!(body["result"]["protocolVersion"], PREFERRED_PROTOCOL_VERSION);
        assert_eq!(body["result"]["capabilities"]["tools"]["listChanged"], true);
        assert_eq!(body["result"]["serverInfo"]["name"], "patchbay");
        assert!(body["result"]["instructions"].is_string());

        // The session is registered and not yet initialized.
        let s = st.sessions.get(&sid).expect("session registered");
        assert!(!s.is_initialized());
    }

    #[tokio::test]
    async fn initialize_echoes_supported_client_version() {
        let st = state();
        let r = jsonrpc::JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(2)),
            method: "initialize".to_string(),
            params: Some(json!({ "protocolVersion": "2025-03-26" })),
        };
        let outcome = dispatch(&st, &r, None, None).await;
        assert_eq!(
            outcome.body.as_ref().unwrap()["result"]["protocolVersion"],
            "2025-03-26"
        );
    }

    #[tokio::test]
    async fn initialize_captures_client_info_name_and_records_it() {
        // (S10) clientInfo.name is stored on the session + recorded in
        // seen_clients on first sight. Route the persist at a temp path so the
        // real config is never touched.
        config::set_test_config_path(Some(config::fresh_test_config_path()));
        let st = state();
        let r = jsonrpc::JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(41)),
            method: "initialize".to_string(),
            params: Some(json!({
                "protocolVersion": "2025-06-18",
                "clientInfo": { "name": "claude-code", "version": "1.2.3" }
            })),
        };
        let outcome = dispatch(&st, &r, None, None).await;
        let sid = outcome.new_session_id.clone().expect("session minted");
        let session = st.sessions.get(&sid).expect("session registered");
        assert_eq!(
            *session.client_name.read(),
            Some("claude-code".to_string()),
            "clientInfo.name stored on the session"
        );
        let cfg = st.config.read();
        assert_eq!(cfg.seen_clients.len(), 1, "recorded on first sight");
        assert_eq!(cfg.seen_clients[0].name, "claude-code");
        assert_eq!(
            cfg.seen_clients[0].first_seen_version.as_deref(),
            Some("1.2.3")
        );
    }

    #[tokio::test]
    async fn initialize_without_client_info_is_tolerated() {
        // (S10) A missing clientInfo leaves client_name None (client falls back
        // to the global list) and records nothing. No config save.
        let st = state();
        let r = req("initialize", Some(json!(42)));
        let outcome = dispatch(&st, &r, None, None).await;
        let sid = outcome.new_session_id.unwrap();
        let session = st.sessions.get(&sid).unwrap();
        assert_eq!(*session.client_name.read(), None);
        assert!(st.config.read().seen_clients.is_empty());
    }

    // ---- S10b: X-Patchbay-Client header takes priority over clientInfo.name ----
    //
    // The user-chosen `X-Patchbay-Client` header (threaded in as the 4th
    // `dispatch` arg) is the PREFERRED identity; clientInfo.name is the
    // fallback. These three tests pin the full priority contract:
    //   1. both present + differ -> header wins;
    //   2. header absent -> clientInfo.name (S10 fallback, unchanged);
    //   3. both absent -> None (S10 behavior, unchanged).

    fn init_req_with_client_info(name: &str, version: Option<&str>) -> jsonrpc::JsonRpcRequest {
        let ci = match version {
            Some(v) => json!({ "name": name, "version": v }),
            None => json!({ "name": name }),
        };
        jsonrpc::JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(60)),
            method: "initialize".to_string(),
            params: Some(json!({ "protocolVersion": "2025-06-18", "clientInfo": ci })),
        }
    }

    #[tokio::test]
    async fn initialize_header_overrides_client_info_name() {
        // Case 1: BOTH the X-Patchbay-Client header AND clientInfo.name are
        // present and DIFFER -> the HEADER value is the resolved identity. The
        // session's client_name AND seen_clients carry the header value, while
        // the version still comes from clientInfo (the header carries only a
        // name/label).
        config::set_test_config_path(Some(config::fresh_test_config_path()));
        let st = state();
        let r = init_req_with_client_info("claude-code", Some("9.9.9"));
        let outcome = dispatch(&st, &r, None, Some("claude-personal")).await;
        let sid = outcome.new_session_id.clone().expect("session minted");
        let session = st.sessions.get(&sid).expect("session registered");
        assert_eq!(
            *session.client_name.read(),
            Some("claude-personal".to_string()),
            "header value takes priority over clientInfo.name"
        );
        let cfg = st.config.read();
        assert_eq!(cfg.seen_clients.len(), 1, "recorded on first sight");
        assert_eq!(
            cfg.seen_clients[0].name, "claude-personal",
            "seen_clients records the HEADER identity, not clientInfo.name"
        );
        assert_eq!(
            cfg.seen_clients[0].first_seen_version.as_deref(),
            Some("9.9.9"),
            "version still comes from clientInfo"
        );
    }

    #[tokio::test]
    async fn initialize_header_absent_falls_back_to_client_info_name() {
        // Case 2: header ABSENT (None) -> clientInfo.name is the identity. This
        // is the graceful no-regression fallback for clients that don't send
        // the header (the entire point of S10b being optional).
        config::set_test_config_path(Some(config::fresh_test_config_path()));
        let st = state();
        let r = init_req_with_client_info("codex", Some("0.1.0"));
        let outcome = dispatch(&st, &r, None, None).await;
        let sid = outcome.new_session_id.clone().expect("session minted");
        let session = st.sessions.get(&sid).expect("session registered");
        assert_eq!(
            *session.client_name.read(),
            Some("codex".to_string()),
            "absent header falls back to clientInfo.name"
        );
        let cfg = st.config.read();
        assert_eq!(cfg.seen_clients.len(), 1);
        assert_eq!(cfg.seen_clients[0].name, "codex");
    }

    #[tokio::test]
    async fn initialize_header_empty_is_treated_as_absent() {
        // An empty/whitespace-only header is treated as absent (trimmed + empty-
        // filtered) -> falls back to clientInfo.name, exactly like case 2.
        config::set_test_config_path(Some(config::fresh_test_config_path()));
        let st = state();
        let r = init_req_with_client_info("antigravity", None);
        let outcome = dispatch(&st, &r, None, Some("   ")).await;
        let sid = outcome.new_session_id.clone().expect("session minted");
        let session = st.sessions.get(&sid).expect("session registered");
        assert_eq!(
            *session.client_name.read(),
            Some("antigravity".to_string()),
            "whitespace-only header is treated as absent -> clientInfo fallback"
        );
    }

    #[tokio::test]
    async fn initialize_no_header_no_client_info_is_none() {
        // Case 3: BOTH absent -> client_name stays None (unchanged S10
        // behavior; client falls back to the global list only). No identity is
        // recorded.
        let st = state();
        let r = req("initialize", Some(json!(61)));
        let outcome = dispatch(&st, &r, None, None).await;
        let sid = outcome.new_session_id.unwrap();
        let session = st.sessions.get(&sid).unwrap();
        assert_eq!(*session.client_name.read(), None);
        assert!(st.config.read().seen_clients.is_empty());
    }

    #[tokio::test]
    async fn initialize_unidentified_client_is_rejected_when_gate_is_on() {
        // (Review fix) An unidentified client (no header, no clientInfo.name)
        // can never be looked up in `forbidden_clients` (is_forbidden(None) is
        // always false), so with the approval gate ON it must be refused
        // outright instead of silently getting default-global access with no
        // prompt and no way to ever forbid it. No session is minted.
        let st = state();
        st.config.write().require_approval_for_new_clients = true;
        let r = req("initialize", Some(json!(62)));
        let outcome = dispatch(&st, &r, None, None).await;
        assert!(outcome.new_session_id.is_none(), "no session minted for a refused connection");
        let body = outcome.body.unwrap();
        assert!(body["error"]["code"].is_i64(), "must be a JSON-RPC error envelope");
        let msg = body["error"]["message"].as_str().unwrap();
        assert!(msg.contains("identify"), "message should explain what's required, got: {msg}");
    }

    #[tokio::test]
    async fn initialized_marks_session_and_returns_202() {
        let st = state();
        let session = st.sessions.create();
        assert!(!session.is_initialized());
        let r = req("notifications/initialized", None);
        let outcome = dispatch(&st, &r, Some(session.clone()), None).await;
        assert_eq!(outcome.http_status, 202);
        assert!(outcome.body.is_none());
        assert!(session.is_initialized());
    }

    #[tokio::test]
    async fn tools_list_with_no_running_jacks_still_shows_meta_tools() {
        // The first-run template ships prod patched:false, so no jack is Running
        // and no upstream tools are merged. Even so, the gateway-owned meta
        // tools (S8) are ALWAYS present.
        let st = state();
        let session = st.sessions.create();
        let r = req("tools/list", Some(json!(3)));
        let outcome = dispatch(&st, &r, Some(session), None).await;
        assert_eq!(outcome.http_status, 200);
        let body = outcome.body.unwrap();
        let tools = body["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        // All four gateway-owned meta tools are present.
        assert!(names.contains(&BUILTIN_ADD_JACK), "names: {:?}", names);
        assert!(names.contains(&BUILTIN_REMOVE_JACK), "names: {:?}", names);
        assert!(names.contains(&BUILTIN_LIST_JACKS), "names: {:?}", names);
        assert!(names.contains(&BUILTIN_TOGGLE_JACK), "names: {:?}", names);
        // No upstream jack tools (prod is unpatched + not running).
        assert!(
            !names.iter().any(|n| n.starts_with("prod__")),
            "unpatched jack must not contribute tools: {:?}",
            names
        );
    }

    #[tokio::test]
    async fn tools_call_unnamespaced_is_invalid_params() {
        // A tool name with no `__` is genuinely malformed -> JSON-RPC
        // invalid_params "unknown tool '<name>'" (D2 protocol-level case).
        let st = state();
        let session = st.sessions.create();
        let r = jsonrpc::JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(5)),
            method: "tools/call".to_string(),
            params: Some(json!({ "name": "echo", "arguments": {} })),
        };
        let outcome = dispatch(&st, &r, Some(session), None).await;
        assert_eq!(outcome.http_status, 200);
        let body = outcome.body.unwrap();
        assert_eq!(body["error"]["code"], jsonrpc::INVALID_PARAMS);
        assert_eq!(body["id"], 5);
    }

    #[tokio::test]
    async fn tools_call_unknown_jack_is_invalid_params() {
        // Namespaced but the jack is not in config -> "unknown tool" (D2 case 1).
        let st = state();
        let session = st.sessions.create();
        let r = jsonrpc::JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(6)),
            method: "tools/call".to_string(),
            params: Some(json!({ "name": "ghost__x", "arguments": {} })),
        };
        let outcome = dispatch(&st, &r, Some(session), None).await;
        let body = outcome.body.unwrap();
        assert_eq!(body["error"]["code"], jsonrpc::INVALID_PARAMS);
        let msg = body["error"]["message"].as_str().unwrap();
        assert!(msg.contains("unknown tool"), "got: {}", msg);
    }

    #[tokio::test]
    async fn tools_call_unpatched_jack_returns_call_tool_error() {
        // first_run_template ships `prod` patched:false. D2 case 2: the call
        // returns a JSON-RPC *result* (not an error) whose CallToolResult is
        // isError with the exact UNPATCHED text. No child is spawned.
        let st = state();
        let session = st.sessions.create();
        let r = jsonrpc::JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(7)),
            method: "tools/call".to_string(),
            params: Some(json!({ "name": "prod__query", "arguments": {} })),
        };
        let outcome = dispatch(&st, &r, Some(session), None).await;
        assert_eq!(outcome.http_status, 200);
        let body = outcome.body.unwrap();
        // It is a JSON-RPC result, not an error object.
        assert!(
            body.get("error").is_none() || body["error"].is_null(),
            "unpatched must NOT be a JSON-RPC error"
        );
        assert_eq!(body["result"]["isError"], true);
        let text = body["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("UNPATCHED"), "text: {}", text);
        assert!(text.contains("prod"), "text must name the jack: {}", text);
        assert!(
            text.contains("Do not retry"),
            "text must direct the model not to retry: {}",
            text
        );
    }

    #[tokio::test]
    async fn tools_call_patched_but_not_running_returns_call_tool_error() {
        // A patched jack that was never started -> D2 case 3 (patched but not
        // Running). No child needed; is_jack_running is false for an absent
        // runtime entry.
        let mut cfg = first_run_template();
        cfg.jacks[0].patched = true; // `prod` patched but never started
        let st = AppState::new(cfg);
        let session = st.sessions.create();
        let r = jsonrpc::JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(8)),
            method: "tools/call".to_string(),
            params: Some(json!({ "name": "prod__query", "arguments": {} })),
        };
        let outcome = dispatch(&st, &r, Some(session), None).await;
        let body = outcome.body.unwrap();
        assert!(
            body.get("error").is_none() || body["error"].is_null(),
            "not-running must NOT be a JSON-RPC error"
        );
        assert_eq!(body["result"]["isError"], true);
        let text = body["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("patched but not running"), "text: {}", text);
        assert!(text.contains("prod"), "text: {}", text);
    }

    #[tokio::test]
    async fn unknown_request_is_method_not_found() {
        // A genuinely unknown method (NOT `ping`, which is now handled — see below).
        let st = state();
        let session = st.sessions.create();
        let r = req("totally/unknown", Some(json!(4)));
        let outcome = dispatch(&st, &r, Some(session), None).await;
        assert_eq!(outcome.http_status, 200);
        let body = outcome.body.unwrap();
        assert_eq!(body["error"]["code"], jsonrpc::METHOD_NOT_FOUND);
        assert_eq!(body["id"], 4);
    }

    #[tokio::test]
    async fn ping_returns_empty_result() {
        // FIX 5: `ping` MUST be answered with an empty result (spec 2025-06-18).
        let st = state();
        let session = st.sessions.create();
        let r = req("ping", Some(json!(11)));
        let outcome = dispatch(&st, &r, Some(session), None).await;
        assert_eq!(outcome.http_status, 200);
        let body = outcome.body.unwrap();
        assert!(body.get("error").is_none() || body["error"].is_null());
        assert_eq!(body["result"], json!({}));
        assert_eq!(body["id"], 11);
    }

    #[tokio::test]
    async fn unknown_notification_is_silent_202() {
        let st = state();
        let session = st.sessions.create();
        let r = req("notifications/anything", None);
        let outcome = dispatch(&st, &r, Some(session), None).await;
        assert_eq!(outcome.http_status, 202);
        assert!(outcome.body.is_none());
    }

    // ---- S8: meta-tool dispatch ----

    /// Helper: dispatch a tools/call and return the CallToolResult fields
    /// (`isError` + first text block) plus whether the response was a JSON-RPC
    /// error (which the meta tools must NEVER produce for validation failures).
    async fn call_meta(method_name: &str, args: Value) -> (bool, Option<String>, bool) {
        let st = state();
        let session = st.sessions.create();
        let r = jsonrpc::JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(21)),
            method: "tools/call".to_string(),
            params: Some(json!({ "name": method_name, "arguments": args })),
        };
        let outcome = dispatch(&st, &r, Some(session), None).await;
        let body = outcome.body.expect("tools/call must have a body");
        let is_jsonrpc_error = body.get("error").map(|e| !e.is_null()).unwrap_or(false);
        let is_error = body["result"]["isError"].as_bool().unwrap_or(false);
        let text = body["result"]["content"][0]["text"]
            .as_str()
            .map(str::to_owned);
        (is_error, text, is_jsonrpc_error)
    }

    #[tokio::test]
    async fn meta_list_jacks_returns_text_summary() {
        // first_run_template has prod (stdio, patched:false, never started).
        let (is_error, text, is_jsonrpc_error) = call_meta(BUILTIN_LIST_JACKS, json!({})).await;
        assert!(!is_jsonrpc_error);
        assert!(!is_error);
        let text = text.expect("list_jacks must return text");
        assert!(text.contains("prod"), "text: {}", text);
        assert!(text.contains("stdio"), "text: {}", text);
    }

    #[tokio::test]
    async fn meta_toggle_unknown_jack_is_call_tool_error() {
        // D2 style: validation failure is a CallToolResult{isError:true}, NOT a
        // JSON-RPC protocol error.
        let (is_error, text, is_jsonrpc_error) =
            call_meta(BUILTIN_TOGGLE_JACK, json!({ "name": "ghost", "patched": true })).await;
        assert!(!is_jsonrpc_error, "must NOT be a JSON-RPC error");
        assert!(is_error);
        let text = text.expect("must have text");
        assert!(text.contains("not found"), "text: {}", text);
    }

    #[tokio::test]
    async fn meta_toggle_jack_missing_patched_is_call_tool_error() {
        let (is_error, text, is_jsonrpc_error) =
            call_meta(BUILTIN_TOGGLE_JACK, json!({ "name": "prod" })).await;
        assert!(!is_jsonrpc_error);
        assert!(is_error);
        assert!(text.unwrap().contains("requires"));
    }

    #[tokio::test]
    async fn meta_add_jack_invalid_name_is_call_tool_error() {
        // Rejected by validation BEFORE config::save -> the real config file is
        // never touched.
        let (is_error, text, is_jsonrpc_error) = call_meta(
            BUILTIN_ADD_JACK,
            json!({ "name": "bad name", "transport": "stdio", "command": "npx" }),
        )
        .await;
        assert!(!is_jsonrpc_error);
        assert!(is_error);
        let text = text.expect("must have text");
        assert!(text.contains("could not add jack"), "text: {}", text);
    }

    #[tokio::test]
    async fn meta_add_jack_missing_transport_is_call_tool_error() {
        // 'transport' absent -> JackConfigInput deserialization fails -> the
        // handler returns a CallToolResult{isError:true} (not a JSON-RPC error).
        let (is_error, _text, is_jsonrpc_error) =
            call_meta(BUILTIN_ADD_JACK, json!({ "name": "newjack" })).await;
        assert!(!is_jsonrpc_error);
        assert!(is_error);
    }

    #[tokio::test]
    async fn meta_remove_unknown_jack_is_call_tool_error() {
        let (is_error, text, is_jsonrpc_error) =
            call_meta(BUILTIN_REMOVE_JACK, json!({ "name": "ghost" })).await;
        assert!(!is_jsonrpc_error);
        assert!(is_error);
        assert!(text.unwrap().contains("not found"));
    }

    #[tokio::test]
    async fn meta_add_jack_is_intercepted_not_routed_as_jack() {
        // The name contains '__' but must be handled as a meta tool, not split
        // into ("patchbay","add_jack") and reported as "unknown tool".
        let (is_error, text, is_jsonrpc_error) = call_meta(
            BUILTIN_ADD_JACK,
            json!({ "name": "bad name", "transport": "stdio", "command": "npx" }),
        )
        .await;
        assert!(!is_jsonrpc_error);
        assert!(is_error);
        let text = text.expect("must have text");
        // It reached add_jack's error text (not the namespace-split "unknown tool").
        assert!(!text.contains("unknown tool"), "text: {}", text);
    }

    // ---- S10: per-client enforcement ----

    /// Build a state where `prod` is patched ON globally but the client "codex"
    /// has an ENABLED override turning `prod` OFF (so effective_patched is false
    /// ONLY for codex). No upstream is ever started.
    fn state_with_codex_override_off() -> AppState {
        use crate::config::{ClientOverride, PatchbayConfig};
        use std::collections::BTreeMap;
        let mut cfg = first_run_template();
        cfg.jacks[0].patched = true; // prod patched ON globally
        let mut jacks = BTreeMap::new();
        jacks.insert("prod".to_string(), false); // ...but OFF for codex
        cfg.client_overrides.insert(
            "codex".to_string(),
            ClientOverride {
                enabled: true,
                jacks,
            },
        );
        AppState::new(cfg)
    }

    fn req_call(jack_tool: &str) -> jsonrpc::JsonRpcRequest {
        jsonrpc::JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(31)),
            method: "tools/call".to_string(),
            params: Some(json!({ "name": jack_tool, "arguments": {} })),
        }
    }

    #[tokio::test]
    async fn tools_call_per_client_override_uses_client_specific_wording() {
        // A session identifying as "codex" sees prod OFF (per-client override)
        // even though prod is patched ON globally -> the unpatched text names the
        // per-agent custom setting, not the global default.
        let st = state_with_codex_override_off();
        let session = st.sessions.create();
        session.set_client_name(Some("codex".to_string()));
        let outcome = dispatch(&st, &req_call("prod__query"), Some(session), None).await;
        let body = outcome.body.unwrap();
        assert_eq!(body["result"]["isError"], true);
        let text = body["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("disabled for THIS agent"), "text: {}", text);
        assert!(text.contains("codex"), "text names the agent: {}", text);
        assert!(
            !text.contains("UNPATCHED"),
            "must NOT use the global wording: {}",
            text
        );
    }

    #[tokio::test]
    async fn tools_call_other_client_still_sees_globally_patched_jack() {
        // A session identifying as a DIFFERENT agent (no override) sees prod ON
        // globally -> NOT blocked at the unpatched stage. It proceeds to the
        // "patched but not running" branch (prod was never started), proving the
        // per-client override did not leak to other clients.
        let st = state_with_codex_override_off();
        let session = st.sessions.create();
        session.set_client_name(Some("claude-code".to_string()));
        let outcome = dispatch(&st, &req_call("prod__query"), Some(session), None).await;
        let body = outcome.body.unwrap();
        assert_eq!(body["result"]["isError"], true);
        let text = body["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("patched but not running"),
            "other client reaches the not-running branch: {}",
            text
        );
    }

    // ---- S10c: forbidden-client enforcement ----

    /// `prod` patched ON globally + a forbidden client "rogue". No upstream is
    /// ever started. Used by the tools/list + tools/call forbidden tests.
    fn state_with_forbidden_client() -> AppState {
        let mut cfg = first_run_template();
        cfg.jacks[0].patched = true; // prod patched ON globally
        cfg.forbidden_clients.push("rogue".to_string());
        AppState::new(cfg)
    }

    #[tokio::test]
    async fn tools_list_forbidden_client_sees_no_tools_at_all() {
        // A forbidden client's tools/list is EMPTY: no jack tools
        // (effective_patched is false for every jack) AND no gateway-owned meta
        // tools ("a forbidden agent should see nothing").
        let st = state_with_forbidden_client();
        let session = st.sessions.create();
        session.set_client_name(Some("rogue".to_string()));
        let r = req("tools/list", Some(json!(70)));
        let outcome = dispatch(&st, &r, Some(session), None).await;
        let body = outcome.body.unwrap();
        let tools = body["result"]["tools"].as_array().unwrap();
        assert!(
            tools.is_empty(),
            "forbidden client must see NO tools (jacks or meta): {:?}",
            tools
        );
    }

    #[tokio::test]
    async fn tools_call_forbidden_client_returns_blocked_error_for_jack_tool() {
        // A jack tool call from a forbidden client -> D2-style
        // CallToolResult{is_error:true} with the "blocked by the user" text
        // (NOT the unpatched/global wording, and NOT a JSON-RPC error).
        let st = state_with_forbidden_client();
        let session = st.sessions.create();
        session.set_client_name(Some("rogue".to_string()));
        let outcome = dispatch(&st, &req_call("prod__query"), Some(session), None).await;
        let body = outcome.body.unwrap();
        assert!(
            body.get("error").is_none() || body["error"].is_null(),
            "forbidden block must NOT be a JSON-RPC error"
        );
        assert_eq!(body["result"]["isError"], true);
        let text = body["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("blocked"), "text: {}", text);
        assert!(text.contains("rogue"), "text names the agent: {}", text);
        assert!(
            !text.contains("UNPATCHED"),
            "must NOT use the unpatched/global wording: {}",
            text
        );
    }

    #[tokio::test]
    async fn tools_call_forbidden_client_blocks_meta_tool_too() {
        // The forbidden gate sits BEFORE the meta-tool interception, so even a
        // gateway-owned meta-tool call is blocked (not routed to the admin path).
        let st = state_with_forbidden_client();
        let session = st.sessions.create();
        session.set_client_name(Some("rogue".to_string()));
        let outcome = dispatch(
            &st,
            &jsonrpc::JsonRpcRequest {
                jsonrpc: "2.0".to_string(),
                id: Some(json!(71)),
                method: "tools/call".to_string(),
                params: Some(json!({ "name": BUILTIN_LIST_JACKS, "arguments": {} })),
            },
            Some(session),
            None,
        )
        .await;
        let body = outcome.body.unwrap();
        assert_eq!(body["result"]["isError"], true);
        let text = body["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("blocked"), "meta tool also blocked: {}", text);
    }

    #[tokio::test]
    async fn forbidden_enforcement_does_not_leak_to_other_clients() {
        // A DIFFERENT (not forbidden) client still sees the meta tools and is
        // NOT blocked by "rogue" being forbidden.
        let st = state_with_forbidden_client();
        let session = st.sessions.create();
        session.set_client_name(Some("claude-code".to_string()));
        let r = req("tools/list", Some(json!(72)));
        let outcome = dispatch(&st, &r, Some(session), None).await;
        let body = outcome.body.unwrap();
        let tools = body["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert!(
            names.contains(&BUILTIN_LIST_JACKS),
            "non-forbidden client still sees meta tools: {:?}",
            names
        );
    }
}
