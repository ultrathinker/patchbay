//! Gateway: hand-rolled minimal MCP over Streamable HTTP (MASTER_PLAN D1/D4).
//!
//! [`run_gateway`] builds the axum router, binds it **strictly** to
//! `127.0.0.1:{port}`, and serves it on Tauri's tokio runtime. On bind failure
//! it logs, records a `Failed` status, and returns (never panics).

pub mod handlers;
pub mod http;
pub mod jsonrpc;
pub mod session;
pub mod sse;
pub mod tools;

use std::net::SocketAddr;

use axum::routing::{delete, post};
use axum::Router;

use crate::app_state::{AppState, GatewayStatus};
use crate::utils::log::log;

/// Build the `/mcp` router: POST (JSON-RPC), GET (SSE), DELETE (session end),
/// all behind the [`http::origin_guard`] middleware. A debug-only
/// `/debug/toggle` route drives the real toggle pipeline so it is curl-testable
/// in DEBUG builds only; it is compiled out of release so the shipped binary
/// has no debug surface (the tray CheckMenuItem is the real trigger in either).
fn build_router(state: AppState) -> Router {
    let router = Router::new()
        .route(
            "/mcp",
            post(http::post_mcp)
                .get(http::get_mcp)
                .delete(http::delete_mcp),
        )
        // Admin jack management (S8): typed REST for non-MCP terminal/script
        // agents. NOT gated behind debug_assertions — ships in release too.
        // Same router, so same origin_guard + strict 127.0.0.1 bind as /mcp.
        .route(
            "/admin/jacks",
            post(http::admin_add_jack).get(http::admin_list_jacks),
        )
        .route("/admin/jacks/{name}", delete(http::admin_remove_jack))
        .route("/admin/jacks/{name}/toggle", post(http::admin_toggle_jack))
        .layer(axum::middleware::from_fn(http::origin_guard));

    // Debug-only test hook (S5): drives the SAME `AppState::set_patched`
    // pipeline as the tray so an automated curl can reproduce a toggle
    // end-to-end. Added BEFORE `with_state` so the handler's `State<AppState>`
    // extractor still resolves. Omitted from release builds.
    #[cfg(debug_assertions)]
    let router = router.route("/debug/toggle", post(http::debug_toggle));

    router.with_state(state)
}

/// Run the gateway on `127.0.0.1:{port}` until the server stops or errors.
///
/// Updates the shared [`GatewayStatus`] (Starting -> Running/Failed -> Stopped).
/// Never panics: a bind error is logged and reflected as `Failed`.
pub async fn run_gateway(state: AppState, port: u16) {
    // Keep handles to the status + upstream startup inputs so we can use them
    // after `state` is moved into the router via `.with_state(state)`.
    let status = state.status.clone();
    // Clone the cheap Arc handles + snapshot the config before `state` moves.
    let upstream = state.upstream.clone();
    let sessions = state.sessions.clone();
    let cfg_arc = state.config.clone();
    let shutdown_gateway = state.shutdown_gateway.clone();

    let addr_str = format!("127.0.0.1:{}", port);
    let addr: SocketAddr = match addr_str.parse() {
        Ok(a) => a,
        Err(e) => {
            log(&format!("gateway: invalid bind address {}: {}", addr_str, e));
            *status.write() = GatewayStatus::Failed {
                reason: format!("invalid bind address {}: {}", addr_str, e),
            };
            return;
        }
    };

    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => {
            log(&format!("gateway: bound to {}", addr));
            l
        }
        Err(e) => {
            log(&format!("gateway: FAILED to bind {}: {}", addr, e));
            *status.write() = GatewayStatus::Failed {
                reason: format!("bind {}: {}", addr, e),
            };
            return;
        }
    };

    *status.write() = GatewayStatus::Running { port };
    log(&format!("gateway: serving MCP on {}", addr));

    // Start patched upstreams OFF the serve path (MASTER_PLAN D4:
    // spawn off the setup/serve thread; decrypt env at spawn). Runs concurrently
    // with serving — the gateway is already bound, so clients connecting after a
    // handshake lands are served immediately.
    tokio::spawn(async move {
        log("upstream: starting patched jacks with unknown runtime");
        let jacks = cfg_arc.read().jacks.clone();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for jack in &jacks {
            // (S10) Start a jack whose shared child SHOULD run: the global flag
            // OR any enabled Custom client needing it (e.g. a jack OFF globally
            // but ON for a persisted Custom client must be ready at boot).
            if !cfg_arc.read().should_run_jack(&jack.name) {
                continue;
            }
            if !seen.insert(jack.name.clone()) {
                log(&format!(
                    "upstream: skipping duplicate jack name '{}' (keeping first)",
                    jack.name
                ));
                continue;
            }
            if upstream.status_string(&jack.name) != "unknown" {
                continue;
            }
            upstream
                .start_jack(jack, sessions.clone(), cfg_arc.clone())
                .await;
        }
        log("upstream: startup pass complete");
    });

    // (FIX 9) Periodically reap idle client sessions (older than 24 h) and
    // enforce a hard cap (500), so abandoned sessions can't accumulate.
    let reaper_sessions = state.sessions.clone();
    let reaper_shutdown_gateway = state.shutdown_gateway.clone();
    tauri::async_runtime::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
        interval.tick().await; // immediate first tick
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    reaper_sessions.reap_idle(std::time::Duration::from_secs(86400), 500);
                }
                _ = reaper_shutdown_gateway.notified() => {
                    log("gateway: session reaper stopped");
                    break;
                }
            }
        }
    });

    // Now `state` is no longer needed for startup -> move it into the router.
    let app = build_router(state);

    let shutdown = async move {
        shutdown_gateway.notified().await;
        log("gateway: graceful shutdown requested");
    };

    if let Err(e) = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
    {
        log(&format!("gateway: server exited with error: {}", e));
        set_status_if_port_matches(
            &status,
            port,
            GatewayStatus::Failed {
                reason: format!("serve error: {}", e),
            },
        );
        return;
    }

    log("gateway: stopped");
    set_status_if_port_matches(&status, port, GatewayStatus::Stopped);
}

fn set_status_if_port_matches(
    status: &parking_lot::RwLock<GatewayStatus>,
    port: u16,
    next: GatewayStatus,
) {
    let mut guard = status.write();
    if matches!(&*guard, GatewayStatus::Running { port: p } if *p == port) {
        *guard = next;
    }
}
