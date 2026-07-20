//! Per-client-session state (MASTER_PLAN D3, authoritative session design).
//!
//! A *session* is created by `initialize`, identified by an `Mcp-Session-Id`
//! (uuid v4 string), and ends on `DELETE /mcp` (or when we drop it).
//!
//! The mutable per-session fields live behind a single `parking_lot::Mutex` so
//! the broadcast path locks only the session's `inner`, snapshots the session
//! list under a short registry read lock, then drops it before iterating (D3).
//! Stage 3 implements the canonical-stream broadcast: registration,
//! generation-guarded unregister, backlog replay/coalescing, and
//! [`SessionRegistry::broadcast_tools_list_changed`].

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};
use serde_json::Value;
use uuid::Uuid;

use crate::gateway::sse::list_changed_notification;

/// A session id is the `Mcp-Session-Id` value: a uuid v4 string.
pub type SessionId = String;

/// Backlog cap: keep at most this many recent records per session (D3 ~32).
const BACKLOG_MAX_RECORDS: usize = 32;
/// Backlog TTL: records older than this are pruned (D3 ~5 min).
const BACKLOG_MAX_AGE: Duration = Duration::from_secs(300);

/// The canonical notification stream registered by a `GET /mcp` (one per
/// session). Replacing it closes the prior sender (generation-guarded).
#[allow(dead_code)]
pub struct StreamSlot {
    /// Unique id of this stream instance (used by the generation guard on drop).
    pub stream_id: Uuid,
    /// Generation counter to reject stale-drop cleanup against a newer reconnect.
    pub generation: u64,
    /// Bounded channel feeding the SSE response writer.
    pub tx: tokio::sync::mpsc::Sender<SseRecord>,
}

/// One SSE event: queued in the per-session backlog, or sent on the canonical
/// stream. `event_id` carries the monotonic `Last-Event-ID` sequence
/// (`"<session_id>:<seq>"`). `Clone`d when copied from the backlog to the wire.
#[allow(dead_code)]
#[derive(Clone)]
pub struct SseRecord {
    pub event_id: String,
    /// Which canonical stream this record was assigned to (`None` = not yet
    /// handed to a live stream; eligible for replay on the next connect).
    pub stream_id: Option<Uuid>,
    pub json: Value,
    pub created_at: Instant,
}

/// Mutable per-session state, guarded by one `Mutex` (D3: the broadcast path
/// locks only this session's `inner`, never the whole registry).
#[allow(dead_code)]
pub struct ClientSessionInner {
    /// The currently-registered canonical SSE stream, if any (`GET /mcp`).
    pub notification_stream: Option<StreamSlot>,
    /// Per-session backlog of records not yet delivered (replay + coalescing).
    pub backlog: VecDeque<SseRecord>,
    /// Monotonic sequence feeding `event_id` / `Last-Event-ID`.
    pub next_event_seq: u64,
    /// Set when a `list_changed` is pending and not yet coalesced onto a stream.
    pub dirty_tools_list: bool,
    /// Last activity timestamp (pruning / debugging).
    pub last_seen: Instant,
}

/// A client session created on `initialize`, shared via `Arc`.
pub struct ClientSession {
    pub id: SessionId,
    /// Set true once `notifications/initialized` arrives.
    pub initialized: AtomicBool,
    /// Negotiated protocol version (e.g. "2025-06-18").
    pub protocol_version: RwLock<String>,
    /// The connecting client's resolved identity. (S10) sourced from
    /// `clientInfo.name`; (S10b) the `X-Patchbay-Client` HTTP header, when
    /// present and non-empty, takes PRIORITY over `clientInfo.name` — letting
    /// the user choose a stable, human-chosen identity from the agent's
    /// connection config. `None` when both are absent (client falls back to the
    /// global list only). Drives per-agent (`client_overrides`) enforcement: a
    /// cheap read snapshots the name, then the live config's
    /// `effective_patched` decides visibility.
    pub client_name: RwLock<Option<String>>,
    /// Mutable per-session state (SSE/backlog/dirty).
    pub inner: Mutex<ClientSessionInner>,
}

impl ClientSession {
    pub fn is_initialized(&self) -> bool {
        self.initialized.load(Ordering::Acquire)
    }

    /// Record the connecting client's resolved identity (S10 + S10b). Called
    /// from `handle_initialize` AFTER the priority resolution: the
    /// `X-Patchbay-Client` header value when present (priority), else
    /// `clientInfo.name`, else `None`. `None` means no identity (client falls
    /// back to the global list only).
    pub fn set_client_name(&self, name: Option<String>) {
        *self.client_name.write() = name;
    }

    /// Register `tx` as this session's canonical notification stream, REPLACING
    /// (and thereby closing) any previously-registered sender (D3).
    ///
    /// Closing the prior sender is exactly how "two GET streams on one session
    /// don't both receive" works: the older stream's channel loses its last
    /// sender and `recv()` returns `None`.
    ///
    /// Returns the fresh `(stream_id, generation)` so the GET handler can
    /// generation-guard its later [`Self::unregister_stream`] on disconnect.
    /// `stream_id` is a brand-new uuid every call, so a stale disconnect
    /// (old `stream_id`) can never match a newer slot regardless of generation.
    pub fn register_stream(
        &self,
        tx: tokio::sync::mpsc::Sender<SseRecord>,
    ) -> (Uuid, u64) {
        let mut inner = self.inner.lock();
        // Per-session generation: strictly greater than the previous slot's
        // generation (or 1 for the first). Derived from the current slot so no
        // extra counter field is needed (the D3 struct shapes stay unchanged).
        let generation = inner
            .notification_stream
            .as_ref()
            .map(|s| s.generation.wrapping_add(1))
            .unwrap_or(1);
        let stream_id = Uuid::new_v4();
        // Assigning a new slot drops the previous `StreamSlot` and with it the
        // previous sender — that closes the older stream's channel.
        inner.notification_stream = Some(StreamSlot {
            stream_id,
            generation,
            tx,
        });
        inner.last_seen = Instant::now();
        (stream_id, generation)
    }

    /// Clear the canonical stream ONLY IF the stored `(stream_id, generation)`
    /// both match (D3 generation guard). A stale disconnect (an older
    /// reconnect's id/generation) must not unregister a newer reconnect.
    pub fn unregister_stream(&self, stream_id: Uuid, generation: u64) {
        let mut inner = self.inner.lock();
        let matches = inner
            .notification_stream
            .as_ref()
            .map(|s| s.stream_id == stream_id && s.generation == generation)
            .unwrap_or(false);
        if matches {
            inner.notification_stream = None;
        }
    }

    /// Bump `last_seen` to now — any inbound activity keeps a session alive
    /// (used by the gateway POST dispatch; FIX 9 idle reaping).
    pub fn touch(&self) {
        self.inner.lock().last_seen = Instant::now();
    }

    /// Compute the backlog records to replay to a freshly-connected stream.
    ///
    /// - `Last-Event-ID` present and parseable (`"<session_id>:<seq>"`): records
    ///   whose sequence is strictly higher than the named one (re-delivery of
    ///   what the client missed on THIS session).
    /// - absent (or unparseable): records never assigned to a live stream
    ///   (`stream_id == None`), TAKEN out of the backlog, PLUS one coalesced
    ///   `list_changed` when `dirty_tools_list` is set (then dirty is cleared).
    pub fn take_replay_since(&self, last_event_id: Option<&str>) -> Vec<SseRecord> {
        let mut inner = self.inner.lock();

        // Case 1: Last-Event-ID present -> replay higher-seq records for THIS
        // session (records whose event_id prefix isn't `self.id` are ignored),
        // PLUS one coalesced list_changed when dirty (list_changed is idempotent;
        // a missing one on a Last-Event-ID reconnect is the bug being fixed).
        if let Some(eid) = last_event_id {
            if let Some(last_seq) = parse_seq(eid) {
                let session_prefix = format!("{}:", self.id);
                let mut out: Vec<SseRecord> = inner
                    .backlog
                    .iter()
                    .filter_map(|r| {
                        if !r.event_id.starts_with(session_prefix.as_str()) {
                            return None;
                        }
                        let rs = parse_seq(&r.event_id)?;
                        if rs > last_seq {
                            Some(SseRecord {
                                event_id: r.event_id.clone(),
                                stream_id: None,
                                json: r.json.clone(),
                                created_at: r.created_at,
                            })
                        } else {
                            None
                        }
                    })
                    .collect();

                // Coalesce a pending list_changed exactly like the no-id path.
                if inner.dirty_tools_list {
                    inner.next_event_seq += 1;
                    let seq = inner.next_event_seq;
                    out.push(SseRecord {
                        event_id: format!("{}:{}", self.id, seq),
                        stream_id: None,
                        json: list_changed_notification(),
                        created_at: Instant::now(),
                    });
                    inner.dirty_tools_list = false;
                }
                return out;
            }
            // Unparseable Last-Event-ID: fall through to the no-id path.
        }

        // Case 2: no usable Last-Event-ID -> take never-assigned records +
        // one coalesced list_changed when dirty.
        let mut out: Vec<SseRecord> = Vec::new();
        // Partition: never-assigned records are taken out (delivered now);
        // assigned records stay as history for a later Last-Event-ID replay.
        let mut keep = VecDeque::with_capacity(inner.backlog.len());
        while let Some(rec) = inner.backlog.pop_front() {
            if rec.stream_id.is_none() {
                out.push(rec);
            } else {
                keep.push_back(rec);
            }
        }
        inner.backlog = keep;

        if inner.dirty_tools_list {
            inner.next_event_seq += 1;
            let seq = inner.next_event_seq;
            out.push(SseRecord {
                event_id: format!("{}:{}", self.id, seq),
                stream_id: None,
                json: list_changed_notification(),
                created_at: Instant::now(),
            });
            inner.dirty_tools_list = false;
        }
        out
    }
}

/// Process-wide registry of live client sessions.
pub struct SessionRegistry {
    sessions: RwLock<HashMap<SessionId, Arc<ClientSession>>>,
}

impl Default for SessionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionRegistry {
    pub fn new() -> Self {
        SessionRegistry {
            sessions: RwLock::new(HashMap::new()),
        }
    }

    /// Create a new (not-yet-initialized) session, register it, and return it.
    /// The id is a fresh uuid v4 string (the `Mcp-Session-Id` header value).
    pub fn create(&self) -> Arc<ClientSession> {
        let session = Arc::new(ClientSession {
            id: Uuid::new_v4().to_string(),
            initialized: AtomicBool::new(false),
            protocol_version: RwLock::new(String::new()),
            client_name: RwLock::new(None),
            inner: Mutex::new(ClientSessionInner {
                notification_stream: None,
                backlog: VecDeque::new(),
                next_event_seq: 1,
                dirty_tools_list: false,
                last_seen: Instant::now(),
            }),
        });
        self.sessions
            .write()
            .insert(session.id.clone(), session.clone());
        session
    }

    /// Look up a live session by id (clones the `Arc`).
    pub fn get(&self, id: &str) -> Option<Arc<ClientSession>> {
        self.sessions.read().get(id).cloned()
    }

    /// (FIX 9) Evict sessions whose `last_seen` is older than `max_age`, then
    /// enforce a hard `cap` by evicting the oldest survivors. Called periodically
    /// by the gateway so abandoned sessions can't accumulate.
    pub fn reap_idle(&self, max_age: Duration, cap: usize) {
        let now = Instant::now();
        // Snapshot (id, last_seen) under a short read lock.
        let entries: Vec<(SessionId, Instant)> = {
            let map = self.sessions.read();
            map.iter()
                .map(|(id, s)| (id.clone(), s.inner.lock().last_seen))
                .collect()
        };

        let mut to_remove: std::collections::HashSet<SessionId> =
            std::collections::HashSet::new();
        for (id, last) in &entries {
            if now.duration_since(*last) > max_age {
                to_remove.insert(id.clone());
            }
        }

        // Hard cap: if the survivors still exceed the cap, evict the oldest.
        let survivors: Vec<&(SessionId, Instant)> =
            entries.iter().filter(|(id, _)| !to_remove.contains(id)).collect();
        if survivors.len() > cap {
            let mut by_age: Vec<&(SessionId, Instant)> = survivors.clone();
            by_age.sort_by_key(|(_, t)| *t);
            let excess = survivors.len().saturating_sub(cap);
            for (id, _) in by_age.into_iter().take(excess) {
                to_remove.insert(id.clone());
            }
        }

        for id in &to_remove {
            self.remove(id);
        }
    }

    /// Remove (and return) a session by id — used by `DELETE /mcp`.
    pub fn remove(&self, id: &str) -> Option<Arc<ClientSession>> {
        let session = self.sessions.write().remove(id);
        // (FIX 11) Close the canonical SSE stream so the client's GET stream
        // ends — a bare DELETE otherwise leaves the channel open.
        if let Some(s) = &session {
            let _ = s.inner.lock().notification_stream.take();
        }
        session
    }

    /// Close every canonical SSE stream without dropping the session records.
    ///
    /// Used before a live gateway rebind: clients connected to the old TCP
    /// listener must see their long-lived GET streams end so axum's graceful
    /// shutdown can complete instead of waiting on SSE responses forever.
    pub fn close_all_streams(&self) {
        let sessions: Vec<Arc<ClientSession>> = {
            let map = self.sessions.read();
            map.values().cloned().collect()
        };
        for session in sessions {
            let _ = session.inner.lock().notification_stream.take();
        }
    }

    /// Test-only helper to flip a session's initialized flag. Production sets
    /// the atomic directly in the `notifications/initialized` handler.
    #[cfg(test)]
    pub fn mark_initialized(&self, id: &str) {
        if let Some(s) = self.get(id) {
            s.initialized.store(true, Ordering::Release);
        }
    }

    /// Broadcast `notifications/tools/list_changed` to every session (D3).
    ///
    /// Snapshots the sessions under a SHORT read lock and DROPS it before
    /// iterating — never hold the registry lock across a per-session lock. For
    /// each session:
    /// - not yet initialized -> set `dirty_tools_list` and skip;
    /// - initialized with a canonical stream -> append an `SseRecord` to its
    ///   backlog BEFORE `try_send`, then send on the canonical stream; on send
    ///   failure (closed/full) clear that stream (if it still matches) and
    ///   leave `dirty` for coalescing on reconnect;
    /// - initialized but disconnected -> set `dirty` (coalesced on next connect).
    ///
    /// The per-session lock is held only briefly; there is no `await` while a
    /// lock is held (D3). `list_changed` is idempotent, so an occasional
    /// duplicate (e.g. a record both `try_send`-ed and in a Last-Event-ID replay
    /// window) is acceptable; a missing one is not.
    pub async fn broadcast_tools_list_changed(&self) {
        // Snapshot Arc<ClientSession>s under a short read lock, then drop it.
        let sessions: Vec<Arc<ClientSession>> = {
            let map = self.sessions.read();
            map.values().cloned().collect()
        };

        let now = Instant::now();

        for session in &sessions {
            if !session.is_initialized() {
                let mut inner = session.inner.lock();
                inner.dirty_tools_list = true;
                continue;
            }

            // Resolve the canonical stream + build the record under one short
            // per-session lock; release before try_send (D3: no await while locked).
            let send: Option<(tokio::sync::mpsc::Sender<SseRecord>, Uuid, SseRecord)> = {
                let mut inner = session.inner.lock();
                inner.last_seen = now;
                let (stream_id, tx) = match &inner.notification_stream {
                    Some(slot) => (slot.stream_id, slot.tx.clone()),
                    None => {
                        // Disconnected: coalesce via dirty; no per-broadcast record.
                        inner.dirty_tools_list = true;
                        continue;
                    }
                };
                inner.next_event_seq += 1;
                let seq = inner.next_event_seq;
                let record = SseRecord {
                    event_id: format!("{}:{}", session.id, seq),
                    stream_id: Some(stream_id),
                    json: list_changed_notification(),
                    created_at: now,
                };
                // Append to backlog BEFORE try_send (D3 ordering).
                inner.backlog.push_back(record.clone());
                prune_backlog(&mut inner.backlog, now);
                Some((tx, stream_id, record))
            };

            if let Some((tx, stream_id, record)) = send {
                if tx.try_send(record).is_err() {
                    // Closed or full: clear the failed stream IF it still matches,
                    // and leave dirty for coalescing on the next reconnect.
                    let mut inner = session.inner.lock();
                    let still_matches = inner
                        .notification_stream
                        .as_ref()
                        .map(|s| s.stream_id == stream_id)
                        .unwrap_or(false);
                    if still_matches {
                        inner.notification_stream = None;
                    }
                    inner.dirty_tools_list = true;
                }
            }
        }
    }
}

/// Parse the monotonic sequence out of an `event_id` of the form
/// `"<session_id>:<seq>"` (the part after the final `:`). Returns `None` for a
/// malformed id so callers can fall back to the no-id replay path.
fn parse_seq(event_id: &str) -> Option<u64> {
    event_id
        .rsplit_once(':')
        .and_then(|(_, seq)| seq.parse::<u64>().ok())
}

/// Prune a session backlog to the last [`BACKLOG_MAX_RECORDS`] records and no
/// older than [`BACKLOG_MAX_AGE`] (D3: ~32 records / ~5 min).
fn prune_backlog(backlog: &mut VecDeque<SseRecord>, now: Instant) {
    while let Some(front) = backlog.front() {
        if now.duration_since(front.created_at) > BACKLOG_MAX_AGE {
            backlog.pop_front();
        } else {
            break;
        }
    }
    while backlog.len() > BACKLOG_MAX_RECORDS {
        backlog.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    #[test]
    fn create_registers_and_gets_back() {
        let reg = SessionRegistry::new();
        let s = reg.create();
        assert!(!s.is_initialized());
        assert!(s.id.len() > 8, "session id should be a uuid string");
        let got = reg.get(&s.id).expect("session must be retrievable");
        assert!(std::ptr::eq(got.as_ref(), s.as_ref()));
    }

    #[test]
    fn mark_initialized_flips_flag() {
        let reg = SessionRegistry::new();
        let s = reg.create();
        assert!(!s.is_initialized());
        reg.mark_initialized(&s.id);
        assert!(s.is_initialized());
    }

    #[test]
    fn remove_drops_session() {
        let reg = SessionRegistry::new();
        let s = reg.create();
        assert!(reg.get(&s.id).is_some());
        let removed = reg.remove(&s.id);
        assert!(removed.is_some());
        assert!(reg.get(&s.id).is_none());
    }

    #[test]
    fn close_all_streams_closes_registered_channels_but_keeps_sessions() {
        let reg = SessionRegistry::new();
        let s = reg.create();
        let (tx, mut rx) = mpsc::channel::<SseRecord>(8);
        s.register_stream(tx);

        reg.close_all_streams();

        assert!(reg.get(&s.id).is_some(), "session must remain registered");
        assert!(
            rx.try_recv().is_err(),
            "channel must be closed after its canonical sender is dropped"
        );
        assert!(s.inner.lock().notification_stream.is_none());
    }

    #[test]
    fn create_yields_distinct_ids() {
        let reg = SessionRegistry::new();
        let a = reg.create();
        let b = reg.create();
        assert_ne!(a.id, b.id);
    }

    // ---- Stage 3 broadcast matrix (D3) ------------------------------------

    #[tokio::test]
    async fn register_replaces_prior_stream_and_generation_guard_holds() {
        // Cases (b) + (d): two streams on one session -> only the latest
        // receives; mid-broadcast replacement neither panics nor double-delivers.
        let reg = SessionRegistry::new();
        let s = reg.create();
        s.initialized.store(true, Ordering::Release);

        // Stream A.
        let (tx_a, mut rx_a) = mpsc::channel::<SseRecord>(8);
        let (id_a, gen_a) = s.register_stream(tx_a);

        // One broadcast -> A receives EXACTLY ONE list_changed.
        reg.broadcast_tools_list_changed().await;
        let rec_a = rx_a.recv().await.expect("A must receive list_changed");
        assert_eq!(rec_a.json["method"], "notifications/tools/list_changed");
        assert!(rx_a.try_recv().is_err(), "A must receive exactly one frame");

        // Register B on the SAME session -> replaces A; A's channel closes.
        let (tx_b, mut rx_b) = mpsc::channel::<SseRecord>(8);
        let (id_b, _gen_b) = s.register_stream(tx_b);
        assert_ne!(id_a, id_b, "each registration mints a fresh stream_id");
        assert!(
            rx_a.recv().await.is_none(),
            "A's channel must close once B registers (canonical replacement)"
        );

        // Next broadcast -> only B receives (no double delivery to A).
        reg.broadcast_tools_list_changed().await;
        let rec_b = rx_b.recv().await.expect("B must receive list_changed");
        assert_eq!(rec_b.json["method"], "notifications/tools/list_changed");

        // Stale drop from A must NOT evict B: another broadcast still lands on B
        // (case d — generation guard, no panic, no double delivery).
        s.unregister_stream(id_a, gen_a);
        reg.broadcast_tools_list_changed().await;
        assert!(
            rx_b.recv().await.is_some(),
            "B must still be canonical after a stale A drop"
        );
    }

    #[tokio::test]
    async fn disconnected_session_coalesces_dirty_into_one_on_connect() {
        // Case (c) prep: a disconnected (initialized) session coalesces many
        // broadcasts into exactly one list_changed on the next connect.
        let reg = SessionRegistry::new();
        let s = reg.create();
        s.initialized.store(true, Ordering::Release);

        // No stream: three broadcasts all coalesce into dirty.
        reg.broadcast_tools_list_changed().await;
        reg.broadcast_tools_list_changed().await;
        reg.broadcast_tools_list_changed().await;

        // Connect with no Last-Event-ID -> exactly one coalesced list_changed.
        let replay = s.take_replay_since(None);
        assert_eq!(replay.len(), 1, "multiple broadcasts coalesce to one");
        assert_eq!(replay[0].json["method"], "notifications/tools/list_changed");
        assert!(!s.inner.lock().dirty_tools_list, "dirty cleared after coalesce");
    }

    #[tokio::test]
    async fn last_event_id_replays_higher_seq_records() {
        // Case (c): a reconnect with Last-Event-ID replays the missed record.
        let reg = SessionRegistry::new();
        let s = reg.create();
        s.initialized.store(true, Ordering::Release);

        // Deliver one live so a backlog record with a real seq exists.
        let (tx, mut rx) = mpsc::channel::<SseRecord>(8);
        s.register_stream(tx);
        reg.broadcast_tools_list_changed().await;
        let rec = rx.recv().await.expect("delivered to canonical stream");

        // Reconnect with Last-Event-ID one below -> that record replays.
        let seq: u64 = parse_seq(&rec.event_id).unwrap();
        let last_id = format!("{}:{}", s.id, seq - 1);
        let replay = s.take_replay_since(Some(&last_id));
        assert_eq!(replay.len(), 1, "the higher-seq record is replayed");
        assert_eq!(replay[0].json["method"], "notifications/tools/list_changed");
    }

    #[tokio::test]
    async fn broadcast_skips_uninitialized_session_but_sets_dirty() {
        // Not-yet-initialized sessions never get a live frame; dirty is set so
        // the change is coalesced once they finish initializing and connect.
        let reg = SessionRegistry::new();
        let s = reg.create();
        reg.broadcast_tools_list_changed().await;
        assert!(s.inner.lock().dirty_tools_list);

        s.initialized.store(true, Ordering::Release);
        let replay = s.take_replay_since(None);
        assert_eq!(replay.len(), 1, "coalesced list_changed delivered post-init");
    }

    #[test]
    fn parse_seq_extracts_trailing_number() {
        assert_eq!(parse_seq("abc-def:42"), Some(42));
        assert_eq!(parse_seq("sess:0"), Some(0));
        assert_eq!(parse_seq("no-colon"), None);
        assert_eq!(parse_seq("sess:notanumber"), None);
    }
}
