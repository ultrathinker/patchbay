// Prevents an additional console window on Windows in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app_state;
mod approval;
mod config;
mod gateway;
mod tray;
mod ui;
mod upstream;
mod utils;
mod window;

use tauri::tray::TrayIconBuilder;
use tauri::Manager; // brings `.manage()` into scope on App
use utils::log::{self, log};

fn main() {
    // macOS: adopt the login shell's PATH so stdio jacks find npx/node/uvx when
    // Patchbay was started from Finder or at login. Must run before ANY thread
    // exists (it calls `set_var`), so even before the logger; logged in setup.
    #[cfg(target_os = "macos")]
    let path_note = utils::shell_env::import_login_path();

    // NOTE: the Level-1 logger is started at the top of `setup`, not here. The
    // single-instance plugin ends a SECOND launch before `setup` runs; starting
    // the logger earlier let that short-lived process rotate the running
    // instance's `patchbay_rCURRENT.log` away (the running one kept writing to
    // the renamed generation). Nothing logs before `setup`.

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


    tauri::Builder::default()
        // (S13) The popover window's IPC surface. Each command is a thin wrapper
        // over ONE `AppState` method — the window is a second view, never a
        // second model. Nothing else is exposed to the page.
        .invoke_handler(tauri::generate_handler![
            ui::ui_snapshot,
            ui::ui_toggle_jack,
            ui::ui_add_jack,
            ui::ui_remove_jack,
            ui::ui_set_client_override,
            ui::ui_enable_custom,
            ui::ui_disable_custom,
            ui::ui_reset_custom_to_global,
            ui::ui_set_forbidden,
            ui::ui_set_forbidden_batch,
            ui::ui_delete_agents,
            ui::ui_undo,
            ui::ui_set_autostart,
            ui::ui_set_require_approval,
            ui::ui_set_request_logging,
            ui::ui_set_ui_mode,
            ui::ui_reload_config,
            ui::ui_retry_gateway,
            ui::ui_open_config,
            ui::ui_open_logs,
            ui::ui_set_port,
            ui::ui_copy_url,
            ui::ui_about,
            ui::ui_close_window,
            ui::ui_quit,
        ])
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // A second launch (Start menu, Finder, Spotlight) means "show me
            // Patchbay": open the popover window at the tray icon.
            log("single-instance: second launch, showing the window");
            tray::show_window_anywhere(app);
        }))
        .setup(move |app| {
            // Level-1 logger first: every later `log(...)` (a `log::info!`
            // wrapper) and the panic hook's `log::error!` need it. Writes
            // `<config dir>/Patchbay/logs/patchbay_rCURRENT.log` (`%APPDATA%`
            // on Windows, `~/Library/Application Support` on macOS) with size
            // rotation + cleanup. Leaked internally so it lives for the run.
            log::init_logger();
            log(&format!("main: Patchbay {} starting", env!("CARGO_PKG_VERSION")));
            #[cfg(target_os = "macos")]
            log(&path_note);
            log("setup: begin");

            // macOS: a menu-bar app — no Dock icon, no app menu, never steals
            // the active-app slot (Info.plist LSUIElement does the same for
            // the bundle before this code runs).
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

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

            // (S13) Popover runtime state: the cached tray rect, the
            // dismissed-by-tray-click flag and the creation-failed latch. Must
            // exist before the first tray event can arrive.
            app.manage(window::PopoverState::default());

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

            // Windows: the full-colour icon. macOS: a monochrome template image
            // that the menu bar tints for light/dark mode and the highlight.
            #[cfg(not(target_os = "macos"))]
            let tray_builder =
                TrayIconBuilder::with_id(tray::TRAY_ID).icon(tauri::include_image!("icons/32x32.png"));
            #[cfg(target_os = "macos")]
            let tray_builder = TrayIconBuilder::with_id(tray::TRAY_ID)
                .icon(tauri::include_image!("icons/tray-template.png"))
                .icon_as_template(true);
            let _tray = tray_builder
                .tooltip(tooltip)
                .menu(&menu)
                .show_menu_on_left_click(true)
                .on_menu_event(tray::on_menu_event)
                .on_tray_icon_event(tray::on_tray_icon_event)
                .build(app)?;

            // (S13 W-D1) Point the icon's buttons at whichever interface the
            // config selects. Default `tray` reapplies exactly what the builder
            // above already set, so an install that never opts in sees no
            // change whatsoever.
            let ui_mode = app_state.config.read().ui_mode;
            tray::apply_ui_mode(&app.handle().clone(), ui_mode);

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

            // Unix: SIGTERM/SIGINT/SIGHUP (logout, `kill`, Ctrl+C in a terminal)
            // must also take the stdio children down. The default action would
            // end the process at once and skip `RunEvent::Exit`.
            #[cfg(unix)]
            {
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    use tokio::signal::unix::{signal, SignalKind};
                    let (Ok(mut term), Ok(mut int), Ok(mut hup)) = (
                        signal(SignalKind::terminate()),
                        signal(SignalKind::interrupt()),
                        signal(SignalKind::hangup()),
                    ) else {
                        log("setup: could not install signal handlers");
                        return;
                    };
                    let name = tokio::select! {
                        _ = term.recv() => "SIGTERM",
                        _ = int.recv() => "SIGINT",
                        _ = hup.recv() => "SIGHUP",
                    };
                    log(&format!("main: {name} received, stopping upstreams and exiting"));
                    upstream::process::kill_all();
                    handle.exit(0);
                });
            }

            log("setup: tray ready");
            log("setup: complete, event loop starting");
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building Patchbay")
        .run(|_app, event| match event {
            // Quit from the window (`app.exit`) or the OS (logout, Cmd+Q on
            // macOS) ends here: kill the stdio children's process groups
            // (macOS/Unix). On Windows the Job Object already does it.
            tauri::RunEvent::Exit => upstream::process::kill_all(),
            // macOS: the user opened Patchbay again while it runs (Finder,
            // Spotlight). LaunchServices activates this process instead of
            // starting a second one, so this is the "second launch" there.
            #[cfg(target_os = "macos")]
            tauri::RunEvent::Reopen { .. } => {
                log("main: reopened by the user, showing the window");
                tray::show_window_anywhere(_app);
            }
            _ => {}
        });
}
