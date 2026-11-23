//! Native confirmation dialogs.
//!
//! One dialog lives here: the "permanently delete this agent" confirmation of
//! the tray's Custom submenu (S12) -- a Win32 `MessageBoxW` on Windows, an
//! AppleScript `display dialog` (run by `/usr/bin/osascript`) on macOS.
//!
//! Until S15 this module also held the first-connection approval dialog. It is
//! gone: an identity Patchbay does not know is now refused with instructions
//! and waits in the list behind
//! [`crate::app_state::AppState::admit_client`] until the user lets it in,
//! instead of stopping the whole desktop with a system-modal question.
//!
//! [`show_delete_confirm_dialog`] is a BLOCKING call: `MessageBoxW` runs its
//! own internal modal message loop and does not return until the user answers.
//! It is therefore NEVER called on the tokio runtime or the tauri main/event
//! thread -- the caller spawns a plain `std::thread` for it.
//!
//! On Windows, `MB_TOPMOST` alone is NOT reliable when the caller is a
//! background/tray-only process with no foreground window of its own --
//! focus-stealing prevention can leave such a dialog sitting minimized on the
//! taskbar, unnoticed for minutes (observed live). The fix has two parts:
//! 1. `AllowSetForegroundWindow(GetCurrentProcessId())` grants THIS process
//!    permission to steal the foreground -- normally reserved for the process
//!    the user is actively interacting with.
//! 2. `MB_SETFOREGROUND` (calls `SetForegroundWindow` on the dialog itself)
//!    plus `MB_SYSTEMMODAL` (topmost + suspends interaction with every other
//!    app system-wide until answered, stronger than plain `MB_TOPMOST`).

// ---- S12: permanently-delete-agent confirmation ----

/// Show the "permanently delete this agent" confirmation dialog for `identity`
/// and block until the user answers. Returns `true` ONLY for an explicit YES
/// (confirm the deletion); `false` for NO or any dialog dismissal (cancel → do
/// nothing).
///
/// The call uses the foreground-stealing flags described in the module docs (so
/// the dialog can't get stranded on the taskbar of a windowless tray process)
/// and `MB_ICONWARNING`, because this is an irreversible purge. The body spells
/// out exactly what deletion removes (known-agents list + any Custom
/// permissions) and the "treated as a new agent on reconnect" consequence, so
/// the user understands the effect before confirming. Only a confirmed YES
/// drives [`AppState::delete_client`]; everything else is a no-op.
///
/// This is a BLOCKING call (`MessageBoxW` runs its own modal message loop and
/// does not return until the user answers), so it is NEVER called on the tokio
/// runtime or the tauri main/event thread -- `on_delete_client_click` spawns a
/// plain `std::thread` for it.
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

    // SAFETY: GetCurrentProcessId takes no pointers and AllowSetForegroundWindow reads
    // a plain DWORD; a failure just means the OS declines the foreground grant
    // (not fatal; MB_SETFOREGROUND/MB_SYSTEMMODAL still push for visibility).
    unsafe {
        let _ = AllowSetForegroundWindow(GetCurrentProcessId());
    }

    // SAFETY: MessageBoxW
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

/// macOS: the same confirmation as an AppleScript dialog with the caution icon.
/// Cancel is the default button, so a stray Return never deletes anything.
#[cfg(target_os = "macos")]
pub fn show_delete_confirm_dialog(identity: &str) -> bool {
    let body = format!(
        "Are you sure you want to permanently delete '{}' from Patchbay?\n\n\
         This removes it from the known agents list and any Custom permissions it \
         had. If it connects again, it will be treated as a new agent.",
        identity
    );
    mac::ask(&body, "Cancel", "Delete", "Cancel", None)
}

/// Fallback for other platforms (none is shipped): DENY-BY-DEFAULT (`false`).
/// A destructive confirm dialog that cannot actually show UI must NEVER default
/// to deleting anything.
#[cfg(not(any(windows, target_os = "macos")))]
pub fn show_delete_confirm_dialog(_identity: &str) -> bool {
    false
}

#[cfg(target_os = "macos")]
mod mac {
    use std::path::Path;
    use std::process::Command;

    use crate::utils::log::log;

    /// Quote `text` as an AppleScript string expression. The identity shown in
    /// the dialog comes from an untrusted `clientInfo.name`, so backslashes and
    /// quotes are escaped (nothing can break out of the literal), control
    /// characters are dropped, and line breaks become `& return &`.
    pub fn applescript_string(text: &str) -> String {
        let parts: Vec<String> = text
            .split('\n')
            .map(|line| {
                let clean: String = line.chars().filter(|c| !c.is_control()).collect();
                let escaped = clean.replace('\\', "\\\\").replace('"', "\\\"");
                format!("\"{escaped}\"")
            })
            .collect();
        parts.join(" & return & ")
    }

    /// Show a two-button dialog and block until the user answers. Returns
    /// `true` only when `ok` was clicked; Cancel, Escape, or any failure to
    /// show the dialog count as `false`.
    pub fn ask(body: &str, cancel: &str, ok: &str, default: &str, icon: Option<&Path>) -> bool {
        let icon = match icon {
            Some(p) => format!("POSIX file {}", applescript_string(&p.to_string_lossy())),
            None => "caution".to_string(),
        };
        let script = format!(
            "activate\ndisplay dialog {body} with title \"Patchbay\" \
             buttons {{{cancel_q}, {ok_q}}} default button {default_q} \
             cancel button {cancel_q} with icon {icon}",
            body = applescript_string(body),
            cancel_q = applescript_string(cancel),
            ok_q = applescript_string(ok),
            default_q = applescript_string(default),
        );
        match Command::new("/usr/bin/osascript").arg("-e").arg(&script).output() {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                if !out.status.success() {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    // -128 is "User canceled": a normal answer, not an error.
                    if !stderr.contains("(-128)") {
                        log(&format!("approval: osascript dialog failed: {}", stderr.trim()));
                    }
                    return false;
                }
                stdout.trim() == format!("button returned:{ok}")
            }
            Err(e) => {
                log(&format!("approval: could not run osascript: {e}"));
                false
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::applescript_string;

        #[test]
        fn quotes_and_backslashes_cannot_break_out() {
            let q = applescript_string(r#"evil" & do shell script "x" & "\"#);
            assert_eq!(q, r#""evil\" & do shell script \"x\" & \"\\""#);
        }

        #[test]
        fn line_breaks_become_return_and_controls_are_dropped() {
            assert_eq!(applescript_string("a\nb\u{7}c"), r#""a" & return & "bc""#);
        }
    }
}
