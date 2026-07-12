// Prevents an additional console window on Windows in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app_state;
mod approval;
mod config;
mod gateway;
mod tray;
mod upstream;
mod utils;

use tauri::tray::TrayIconBuilder;
use tauri::Manager; // brings `.manage()` into scope on App
use utils::log::{self, log};

fn main() {
    // Level-1 logger FIRST: every later `log(...)` (which delegates to
    // `log::info!`) and the panic hook's `log::error!` depend on flexi_logger
    // being installed. Writes to `%APPDATA%\Patchbay\logs\patchbay_rCURRENT.log`
    // with size rotation + cleanup. Leaked internally so it lives for the run.
    log::init_logger();

    // Panic hook: log the panic (message + location) via the same Level-1
    // logger so a crash leaves a trace in patchbay_rCURRENT.log, THEN chain the
    // previous (default) hook so stderr output is unchanged. Written with
    // WriteMode::Direct so the line is flushed before the release profile's
    // panic=abort terminates the process. (The old hook wrote a separate
    // patchbay.crash.log directly; that is superseded by the unified logger.)
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // `::log::` (absolute) because this file's `use utils::log::{self, log}`
        // shadows the bare `log` name with our own module.
        ::log::error!("PANIC: {}", info);
        default_hook(info);
    }));

    log("main: Patchbay starting (stage 5 enforcement + tray toggles)");

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|_app, _args, _cwd| {
            // No window in v0.1, so there is nothing to focus/show. A second
            // launch just logs and no-ops.
            log("single-instance: second launch ignored (no window in v0.1)");
        }))
        .setup(|app| {
            log("setup: begin");

            // ---- Config (S1): load once, log a summary, surface violations ----
            // Light work (one small JSON read), done synchronously on the setup
            // thread. Heavier startup (upstream spawn, gateway bind) stays off
            // this thread.
            //
            // (FIX 4) load_result distinguishes missing (first run) from a
            // corrupt file: a corrupt config is NEVER overwritten — we fall back
            // to a safe default in memory only and surface the error in the tray.
            let (cfg, config_err): (config::PatchbayConfig, Option<String>) =
                match config::load_result() {
                    Ok(c) => (c, None),
                    Err(config::ConfigError::Missing) => {
                        // First run: load() writes the template and returns it.
                        (config::load(), None)
                    }
                    Err(e) => {
                        let reason = e.reason();
                        log(&format!(
                            "config: corrupt config, using safe default (NOT saved): {}",
                            reason
                        ));
                        (config::safe_default(), Some(reason))
                    }
                };
            let patched = cfg.jacks.iter().filter(|j| j.patched).count();
            log(&format!(
                "config: loaded {} jacks, {} patched, port {}",
                cfg.jacks.len(),
                patched,
                cfg.port
            ));
            if let Err(violations) = config::validate(&cfg) {
                for v in &violations {
                    log(&format!("config: INVALID - {}", v));
                }
            }

            // ---- App state + gateway (S2/S4) ----
            let port = cfg.port;
            let app_state = app_state::AppState::new(cfg);
            if let Some(reason) = &config_err {
                *app_state.config_error.write() = Some(reason.clone());
            }
            app.manage(app_state.clone());

            // Level-2 lifecycle: record the app start (no-op unless request
            // logging is on).
            utils::request_log::log_event(&app_state, "app_start");

            // The tray handle map must exist before build_menu populates it.
            app.manage(tray::TrayItems::default());

            // Start the gateway OFF the setup thread so the tray stays
            // responsive from the first frame.
            let gw_state = app_state.clone();
            tauri::async_runtime::spawn(async move {
                gateway::run_gateway(gw_state, port).await;
            });

            // ---- Real tray menu (S5): one CheckMenuItem per jack ----
            // Per-jack checks drive the shared toggle pipeline (see `tray.rs`).
            let handle = app.handle();
            let menu = tray::build_menu(handle, &app_state)?;
            let tooltip = tray::tooltip_text(handle);

            let _tray = TrayIconBuilder::with_id(tray::TRAY_ID)
                .icon(tauri::include_image!("icons/32x32.png"))
                .tooltip(tooltip)
                .menu(&menu)
                .show_menu_on_left_click(true)
                .on_menu_event(tray::on_menu_event)
                .build(app)?;

            // (S10) Inject the app handle into AppState so a background task
            // (recording a newly-seen MCP client from the gateway path) can
            // rebuild the tray menu + refresh the tooltip.
            app_state.set_tray_handle(app.handle().clone());

            // S7: the gateway task sets GatewayStatus::Failed on a bind error
            // (port conflict) asynchronously. Refresh the tooltip shortly after
            // startup so that failure — or a healthy Running — is reflected in
            // the tray. "Retry gateway" in the tray re-attempts the bind.
            let refresh_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(750)).await;
                tray::refresh_tooltip(&refresh_handle);
            });

            log("setup: tray ready");
            log("setup: complete, event loop starting");
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running Patchbay");
}
