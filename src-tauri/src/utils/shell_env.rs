//! macOS: give Patchbay the user's real `PATH`.
//!
//! An app started from Finder, the Dock or a LaunchAgent gets launchd's minimal
//! `PATH` (`/usr/bin:/bin:/usr/sbin:/sbin`), so a stdio jack whose command is
//! `npx`, `node`, `uvx` or `docker` would fail to spawn even though it works in
//! the user's terminal. At startup we ask the user's login shell for its `PATH`
//! (the same thing VS Code and other GUI tools do) and add the usual install
//! locations as a fallback, so jacks resolve commands exactly as a terminal
//! would.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const MARK: &str = "__PATCHBAY_PATH__";
const TIMEOUT: Duration = Duration::from_secs(5);

/// Ask `$SHELL -l -i` for its PATH. `None` on any failure or after
/// [`TIMEOUT`] (a slow or interactive shell profile must not stall startup).
fn login_shell_path() -> Option<String> {
    let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/zsh".into());
    let mut child = Command::new(&shell)
        .args(["-l", "-i", "-c", &format!("printf '{MARK}%s{MARK}' \"$PATH\"")])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() < TIMEOUT => std::thread::sleep(Duration::from_millis(25)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    let start = out.find(MARK)? + MARK.len();
    let len = out[start..].find(MARK)?;
    let path = out[start..start + len].trim().to_string();
    (!path.is_empty()).then_some(path)
}

/// Merge PATH lists in order, dropping empty and duplicate entries.
pub(crate) fn merge_paths(lists: &[&str]) -> String {
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<&str> = Vec::new();
    for list in lists {
        for dir in list.split(':') {
            if !dir.is_empty() && seen.insert(dir) {
                out.push(dir);
            }
        }
    }
    out.join(":")
}

/// Replace this process's PATH with the login shell's, plus the current one
/// and common install locations. Call it first thing in `main`, before any
/// thread is started — even the logger's (it uses `std::env::set_var`).
/// Returns the line to log once the logger is up.
pub fn import_login_path() -> String {
    let current = std::env::var("PATH").unwrap_or_default();
    let home = dirs::home_dir().map(|h| h.to_string_lossy().to_string()).unwrap_or_default();
    let fallback = [
        "/opt/homebrew/bin",
        "/opt/homebrew/sbin",
        "/usr/local/bin",
        &format!("{home}/.local/bin"),
        &format!("{home}/.cargo/bin"),
        &format!("{home}/.bun/bin"),
        &format!("{home}/.volta/bin"),
    ]
    .join(":");
    let shell = login_shell_path();
    let merged = merge_paths(&[shell.as_deref().unwrap_or(""), &current, &fallback]);
    // Single-threaded at this point (see the doc comment).
    std::env::set_var("PATH", &merged);
    format!(
        "main: PATH {} the login shell: {}",
        if shell.is_some() { "imported from" } else { "NOT available from" },
        merged
    )
}

#[cfg(test)]
mod tests {
    use super::merge_paths;

    #[test]
    fn merge_keeps_order_and_drops_duplicates() {
        assert_eq!(
            merge_paths(&["/a:/b", "/b:/c:", ":/a:/d"]),
            "/a:/b:/c:/d"
        );
    }
}
