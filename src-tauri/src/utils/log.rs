//! Level-1 diagnostic logging for Patchbay.
//!
//! Always-on internal log (`patchbay_rCURRENT.log`, etc.) written via the
//! [`flexi_logger`] facade. Replaces an earlier hand-rolled writer; the public
//! [`log`] entry point is preserved verbatim so the ~12 existing call sites
//! (`log("...")`) keep working unchanged — it now delegates to
//! `log::info!(...)`, which `flexi_logger` routes to a size-rotated file.
//!
//! Files live under `%APPDATA%\Patchbay\logs`. Rotation happens at 25 MB
//! (matching the old cap) but [`Cleanup::KeepLogFiles`] now keeps up to 5
//! generations instead of the old single `.old` copy, so total on-disk use is
//! capped at ~125 MB while never truncating recent history mid-run.
//!
//! The spec scopes logging to the `patchbay` module only (`LogSpecBuilder::new`
//! starts with everything off, then enables `patchbay=info`), so third-party
//! crates (reqwest, tokio, axum, hyper) that emit through the `log` facade
//! don't spam our diagnostic file. `WriteMode::Direct` writes + flushes each
//! line synchronously, which matters because the release profile uses
//! `panic = "abort"`: buffered/async writers would lose the last line on abort.
//!
//! [`Cleanup::KeepLogFiles`]: flexi_logger::Cleanup::KeepLogFiles

use std::path::PathBuf;

/// Resolve the Level-1 logs directory `%APPDATA%\Patchbay\logs`, creating it.
/// Shared by [`init_logger`] (Level 1 files) and the request log (Level 2,
/// which nests a `requests/` subdir under this).
pub fn logs_dir() -> PathBuf {
    let mut dir = dirs::config_dir().unwrap_or_else(std::env::temp_dir);
    dir.push("Patchbay");
    dir.push("logs");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Max size of the active Level-1 log before `flexi_logger` rotates it (bytes).
/// Matches the previous hand-rolled cap; `KeepLogFiles(5)` bounds total use.
const LOG_MAX_BYTES: u64 = 25 * 1024 * 1024;

/// How many rotated Level-1 generations to keep (the active `_rCURRENT` file is
/// in addition to these). The old writer kept only one `.old` copy.
const LOG_KEEP_FILES: usize = 5;

/// Initialize the Level-1 logger exactly once at process startup.
///
/// Installs a `flexi_logger` `FileLogWriter` (basename `patchbay`, size-rotated
/// at [`LOG_MAX_BYTES`], keeping [`LOG_KEEP_FILES`] generations) as the global
/// `log` facade. The returned [`LoggerHandle`] is deliberately leaked:
/// `flexi_logger` flushes + shuts down its file writer when the handle is
/// dropped, so for a long-lived tray daemon we keep it alive until process exit.
///
/// Safe to call before the panic hook is installed; `log::error!` from the
/// panic hook then lands in the active file (flushed synchronously via
/// `WriteMode::Direct`).
pub fn init_logger() {
    let mut spec = flexi_logger::LogSpecBuilder::new(); // all logging OFF
    spec.module("patchbay", log::LevelFilter::Info); // only OUR crate at info+

    let result = flexi_logger::Logger::with(spec.build())
        .log_to_file(
            flexi_logger::FileSpec::default()
                .directory(logs_dir())
                .basename("patchbay"),
        )
        .rotate(
            flexi_logger::Criterion::Size(LOG_MAX_BYTES),
            flexi_logger::Naming::Numbers,
            flexi_logger::Cleanup::KeepLogFiles(LOG_KEEP_FILES),
        )
        // Direct (sync write+flush per line): the release profile uses
        // panic=abort, so a buffered/async writer could drop the last message
        // (including a panic trace) on the floor.
        .write_mode(flexi_logger::WriteMode::Direct)
        .start();

    match result {
        Ok(handle) => {
            // Leak: keep the FileLogWriter alive until the process exits.
            std::mem::forget(handle);
        }
        Err(e) => {
            eprintln!("flexi_logger: failed to start: {}", e);
        }
    }
}

/// Write one diagnostic line to the Level-1 log. The existing entry point used
/// everywhere in the codebase (~12 call sites); now a thin wrapper over the
/// `log` facade so `flexi_logger` owns rotation + formatting + cleanup. The
/// caller's single-string-message contract is unchanged.
pub fn log(msg: &str) {
    log::info!("{msg}");
}
