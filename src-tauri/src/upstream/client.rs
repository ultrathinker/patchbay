//! Transport-agnostic upstream MCP client (MASTER_PLAN S6 / D4).
//!
//! `UpstreamClient` is the single interface the `UpstreamManager` calls on a
//! Running jack's connection, regardless of transport. Today there are two
//! implementations:
//! - [`crate::upstream::stdio::StdioClient`] — newline-delimited JSON-RPC over a
//!   spawned child's stdio.
//! - [`crate::upstream::http::HttpClient`] — Streamable HTTP (JSON or SSE
//!   responses) to a remote MCP server.
//!
//! `JackRuntime.client` holds `Option<Arc<dyn UpstreamClient>>`; both transports
//! coerce into it. [`ClientEvent`] is shared between the two transports and the
//! manager's per-jack supervisor (it used to live in `stdio.rs`).

use serde_json::Value;

/// Signals emitted by a transport's reader/supervisor path for the manager's
/// per-jack supervisor to act on. Both transports surface these on the channel
/// returned from spawn/connect.
#[derive(Debug, Clone)]
pub enum ClientEvent {
    /// Upstream sent `notifications/tools/list_changed` (refresh the cache).
    ToolsListChanged,
    /// The connection is gone — the child exited, the remote stream closed, or
    /// shutdown was requested (mark Failed / Stopped).
    Exited(String),
}

/// A connected upstream MCP client, transport-agnostic. The manager holds one
/// `Arc<dyn UpstreamClient>` per Running jack.
///
/// Method semantics mirror [`crate::upstream::stdio::StdioClient`]:
/// - `initialize` performs the MCP handshake (must precede the others);
/// - `list_tools` returns the raw tool definitions (names NOT yet namespaced —
///   the gateway merges + namespaces);
/// - `call_tool` returns the upstream's `result` object (the `CallToolResult`,
///   including any `isError` it set);
/// - `shutdown` is best-effort and must guarantee that no further outbound
///   requests reach the upstream (enforcement: unpatched ⇒ no reachability).
#[async_trait::async_trait]
pub trait UpstreamClient: Send + Sync {
    /// MCP handshake (`initialize` + `notifications/initialized`).
    async fn initialize(&self) -> Result<(), String>;
    /// Fetch the upstream's tool list.
    async fn list_tools(&self) -> Result<Vec<Value>, String>;
    /// Forward a `tools/call` to the upstream.
    async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, String>;
    /// Best-effort graceful shutdown. Must stop all outbound traffic.
    async fn shutdown(&self);
}
