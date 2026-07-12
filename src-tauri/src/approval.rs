//! First-connection approval dialog for unknown MCP agents (S10c).
//!
//! When a client identity connects to Patchbay for the FIRST TIME (not yet in
//! `seen_clients`, not already `forbidden_clients`, and the global
//! `require_approval_for_new_clients` gate is ON), a native Win32 modal asks the
//! user "Allow or deny?". The result drives the config decision in
//! [`crate::app_state::AppState::ensure_client_approved`].
//!
//! [`show_approval_dialog`] is a BLOCKING call: `MessageBoxW` runs its own
//! internal modal message loop and does not return until the user answers. It
//! is therefore NEVER called on the tokio runtime — [`AppState`] calls it from a
//! plain `std::thread::spawn` and signals the result back to the awaiting async
//! task (the `initialize` request that triggered it) via a
//! `tokio::sync::watch` channel (see `ensure_client_approved`).
//!
//! The actual dialog cannot be unit-tested without a live Windows session, so
//! the DECISION LOGIC (allow → seen, deny → forbidden; the concurrency dedup)
//! lives separately in `AppState` and is unit-tested there. This module is the
//! live-verification-only side effect.

/// Show the first-connection approval dialog for `identity` and block until the
/// user answers. Returns `true` for Allow (Yes), `false` for Deny (No).
///
/// `MB_TOPMOST` alone is NOT reliable when the caller is a background/tray-only
/// process with no foreground window of its own — Windows' focus-stealing
/// prevention can leave a `MB_TOPMOST`-only dialog sitting minimized on the
/// taskbar, unnoticed for minutes (observed live). The fix has two parts:
/// 1. `AllowSetForegroundWindow(GetCurrentProcessId())` grants THIS process
///    permission to steal the foreground — normally reserved for the process
///    the user is actively interacting with.
/// 2. `MB_SETFOREGROUND` (calls `SetForegroundWindow` on the dialog itself)
///    plus `MB_SYSTEMMODAL` (topmost + suspends interaction with every other
///    app system-wide until answered, stronger than plain `MB_TOPMOST`).
#[cfg(windows)]
pub fn show_approval_dialog(identity: &str) -> bool {
    use windows::core::HSTRING;
    use windows::Win32::System::Threading::GetCurrentProcessId;
    use windows::Win32::UI::WindowsAndMessaging::{
        AllowSetForegroundWindow, IDYES, MB_ICONQUESTION, MB_SETFOREGROUND, MB_SYSTEMMODAL,
        MB_TOPMOST, MB_YESNO, MESSAGEBOX_STYLE, MessageBoxW,
    };

    let body = format!(
        "'{}' wants to connect to Patchbay for the first time.\n\n\
         Allow this AI agent to access your configured MCP servers?",
        identity
    );

    // YES = Allow, NO = Deny. The OR of these MESSAGEBOX_STYLE flags is a
    // single value passed as the `uType` argument.
    let style: MESSAGEBOX_STYLE =
        MB_YESNO | MB_ICONQUESTION | MB_SYSTEMMODAL | MB_TOPMOST | MB_SETFOREGROUND;

    // SAFETY: GetCurrentProcessId takes no pointers; AllowSetForegroundWindow
    // reads a plain DWORD. Both are infallible from Rust's perspective (a
    // failure just means the OS declines the foreground grant — not fatal,
    // MB_SETFOREGROUND/MB_SYSTEMMODAL below still push hard for visibility).
    unsafe {
        let _ = AllowSetForegroundWindow(GetCurrentProcessId());
    }

    // SAFETY: MessageBoxW reads two wide strings (built via HSTRING, which is
    // NUL-terminated UTF-16) and a flags word; a null owner (`None`) is the
    // documented windowless usage. The call blocks, running its own modal
    // message loop until the user clicks — which is exactly why it runs on a
    // std thread, not the tokio runtime (see the module docs).
    let result = unsafe {
        MessageBoxW(
            None,
            &HSTRING::from(body),
            &HSTRING::from("Patchbay"),
            style,
        )
    };

    result == IDYES
}

/// Non-Windows fallback (the shipped binary is Windows-only, but a stub keeps
/// non-Windows compiles/tests green): default to ALLOW so the app still
/// functions without a dialog. The real decision logic in `AppState` is the part
/// under test; this side effect is live-verification-only on Windows anyway.
#[cfg(not(windows))]
pub fn show_approval_dialog(_identity: &str) -> bool {
    true
}

// ---- S12: permanently-delete-agent confirmation ----

/// Show the "permanently delete this agent" confirmation dialog for `identity`
/// and block until the user answers. Returns `true` ONLY for an explicit YES
/// (confirm the deletion); `false` for NO or any dialog dismissal (cancel → do
/// nothing).
///
/// This is the destructive counterpart to [`show_approval_dialog`]: the SAME
/// `MessageBoxW` call shape + foreground-stealing flags (so the dialog can't
/// get stranded on the taskbar of a windowless tray process), but
/// [`MB_ICONWARNING`] instead of [`MB_ICONQUESTION`] — this is an
/// irreversible/purge action, not a routine allow/deny question. The body spells
/// out exactly what deletion removes (known-agents list + any Custom
/// permissions) and the "treated as a new agent on reconnect" consequence, so
/// the user understands the effect before confirming. Only a confirmed YES
/// drives [`AppState::delete_client`]; everything else is a no-op.
///
/// Like [`show_approval_dialog`], this is a BLOCKING call (`MessageBoxW` runs
/// its own modal message loop and does not return until the user answers), so
/// it is NEVER called on the tokio runtime or the tauri main/event thread —
/// `on_delete_client_click` spawns a plain `std::thread` for it (mirroring how
/// `ensure_client_approved` off-loads [`show_approval_dialog`]).
#[cfg(windows)]
pub fn show_delete_confirm_dialog(identity: &str) -> bool {
    use windows::core::HSTRING;
    use windows::Win32::System::Threading::GetCurrentProcessId;
    use windows::Win32::UI::WindowsAndMessaging::{
        AllowSetForegroundWindow, IDYES, MB_ICONWARNING, MB_SETFOREGROUND, MB_SYSTEMMODAL,
        MB_TOPMOST, MB_YESNO, MESSAGEBOX_STYLE, MessageBoxW,
    };

    let body = format!(
        "Are you sure you want to permanently delete '{}' from Patchbay?\n\n\
         This removes it from the known agents list and any Custom permissions it \
         had. If it connects again, it will be treated as a new agent.",
        identity
    );

    // YES = confirm the deletion, NO/cancel = abort. MB_ICONWARNING (not
    // MB_ICONQUESTION) because this is a destructive, irreversible action.
    let style: MESSAGEBOX_STYLE =
        MB_YESNO | MB_ICONWARNING | MB_SYSTEMMODAL | MB_TOPMOST | MB_SETFOREGROUND;

    // SAFETY: same shape/justification as show_approval_dialog —
    // GetCurrentProcessId takes no pointers and AllowSetForegroundWindow reads
    // a plain DWORD; a failure just means the OS declines the foreground grant
    // (not fatal; MB_SETFOREGROUND/MB_SYSTEMMODAL still push for visibility).
    unsafe {
        let _ = AllowSetForegroundWindow(GetCurrentProcessId());
    }

    // SAFETY: same shape/justification as show_approval_dialog — MessageBoxW
    // reads two NUL-terminated wide strings (via HSTRING) + a flags word, with
    // a null owner (`None`) for the documented windowless usage. Blocks on its
    // own modal loop until the user clicks — hence the std-thread off-load.
    let result = unsafe {
        MessageBoxW(
            None,
            &HSTRING::from(body),
            &HSTRING::from("Patchbay"),
            style,
        )
    };

    result == IDYES
}

/// Non-Windows fallback (live binary is Windows-only; the stub keeps non-Windows
/// compiles/tests green): DENY-BY-DEFAULT (`false`). Unlike the ALLOW-by-default
/// [`show_approval_dialog`] stub, a destructive confirm dialog that cannot
/// actually show UI must NEVER default to deleting anything — a stub that can't
/// ask should refuse the irreversible action.
#[cfg(not(windows))]
pub fn show_delete_confirm_dialog(_identity: &str) -> bool {
    false
}
