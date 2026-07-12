//! SSE framing for the canonical per-session notification stream (MASTER_PLAN D3).
//!
//! - [`list_changed_notification`]: the JSON-RPC body of
//!   `notifications/tools/list_changed` (the only server-initiated message v0.1
//!   broadcasts).
//! - [`event_from_record`]: serialize an [`SseRecord`] into an axum SSE `Event`
//!   (`event: message`, `data: <compact json>`, `id: <event_id>`).
//! - [`GuardedStream`]: wraps the live `SseRecord` stream so that, when the
//!   client disconnects (axum drops the stream), the canonical stream is
//!   **generation-guarded** unregistered from its session — a stale disconnect
//!   can never evict a newer reconnect (D3).

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::response::sse::Event;
use futures::Stream;
use serde_json::{json, Value};

use crate::gateway::session::{ClientSession, SseRecord};

/// The JSON-RPC body of `notifications/tools/list_changed`.
pub fn list_changed_notification() -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "notifications/tools/list_changed"
    })
}

/// Serialize an [`SseRecord`] into an axum SSE `Event`:
/// `event: message`, `data: <compact json string>`, `id: <event_id>`.
///
/// Always succeeds (`Infallible`); a record that fails to serialize emits a
/// minimal `{}` data frame so the stream never stalls.
pub fn event_from_record(record: &SseRecord) -> Result<Event, Infallible> {
    let data = serde_json::to_string(&record.json).unwrap_or_else(|_| "{}".to_string());
    Ok(Event::default()
        .event("message")
        .data(data)
        .id(record.event_id.clone()))
}

/// A stream wrapper that, on drop, **generation-guarded** unregisters its
/// canonical stream from the owning session (D3).
///
/// When the client disconnects, axum drops the `Sse` response and therefore
/// this stream; the `Drop` impl calls
/// [`ClientSession::unregister_stream`] with the `(stream_id, generation)`
/// captured at registration. Because `unregister_stream` only clears the slot
/// when BOTH match, a stale drop from an older reconnect cannot evict a newer
/// one.
pub struct GuardedStream<S> {
    inner: S,
    session: Arc<ClientSession>,
    stream_id: uuid::Uuid,
    generation: u64,
}

impl<S> GuardedStream<S> {
    /// Wrap `inner` so its eventual drop unregisters `(stream_id, generation)`
    /// from `session`.
    pub fn new(
        inner: S,
        session: Arc<ClientSession>,
        stream_id: uuid::Uuid,
        generation: u64,
    ) -> Self {
        GuardedStream {
            inner,
            session,
            stream_id,
            generation,
        }
    }
}

impl<S> Drop for GuardedStream<S> {
    fn drop(&mut self) {
        self.session
            .unregister_stream(self.stream_id, self.generation);
    }
}

impl<S> Stream for GuardedStream<S>
where
    S: Stream + Unpin,
{
    type Item = S::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll_next(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn list_changed_body_is_a_jsonrpc_notification() {
        let v = list_changed_notification();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["method"], "notifications/tools/list_changed");
        // A notification has no id/result/error.
        assert!(v.get("id").is_none() || v["id"].is_null());
    }

    #[test]
    fn event_from_record_carries_message_event_data_and_id() {
        let rec = SseRecord {
            event_id: "sess:7".to_string(),
            stream_id: None,
            json: list_changed_notification(),
            created_at: Instant::now(),
        };
        let ev = event_from_record(&rec).unwrap();
        let text = format!("{:?}", ev);
        // The Event debug repr carries the field values we set.
        assert!(text.contains("message"), "event type must be 'message'");
        assert!(text.contains("sess:7"), "event id must be carried");
        assert!(
            text.contains("notifications/tools/list_changed"),
            "data must be the compact json"
        );
    }
}
