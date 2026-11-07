//! Start Patchbay at login.
//!
//! - Windows: the HKCU `Run` key, value `Patchbay` (quoted exe path).
//! - macOS: a per-user LaunchAgent, `~/Library/LaunchAgents/dev.patchbay.app.plist`,
//!   that runs the current executable at login. Writing the file does not start
//!   a second copy now; it takes effect at the next login. macOS lists it under
//!   System Settings > General > Login Items as a background item of Patchbay.

pub fn set_autorun(enable: bool) -> Result<(), String> {
    imp::set_autorun(enable)
}

#[cfg(windows)]
mod imp {
    use winreg::enums::*;
    use winreg::RegKey;

    pub fn set_autorun(enable: bool) -> Result<(), String> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let path = r"Software\Microsoft\Windows\CurrentVersion\Run";
        let key = hkcu
            .open_subkey_with_flags(path, KEY_WRITE)
            .map_err(|e| format!("Failed to open registry key: {}", e))?;

        if enable {
            let exe_path =
                std::env::current_exe().map_err(|e| format!("Failed to get exe path: {}", e))?;

            // Quote the path so spaces (e.g. C:\Program Files\...) parse unambiguously.
            let value = format!("\"{}\"", exe_path.to_string_lossy());
            key.set_value("Patchbay", &value)
                .map_err(|e| format!("Failed to set registry value: {}", e))?;
        } else {
            key.delete_value("Patchbay").ok();
        }

        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::path::PathBuf;

    const LABEL: &str = "dev.patchbay.app";

    fn plist_path() -> Result<PathBuf, String> {
        let home = dirs::home_dir().ok_or("no home directory")?;
        Ok(home.join("Library/LaunchAgents").join(format!("{LABEL}.plist")))
    }

    /// Escape text for an XML string element.
    fn xml_escape(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&apos;")
    }

    pub(super) fn plist_for(exe: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>ProcessType</key>
    <string>Interactive</string>
    <key>LimitLoadToSessionType</key>
    <string>Aqua</string>
    <key>AssociatedBundleIdentifiers</key>
    <array>
        <string>{LABEL}</string>
    </array>
</dict>
</plist>
"#,
            exe = xml_escape(exe)
        )
    }

    pub fn set_autorun(enable: bool) -> Result<(), String> {
        let path = plist_path()?;
        if enable {
            let exe =
                std::env::current_exe().map_err(|e| format!("Failed to get exe path: {}", e))?;
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)
                    .map_err(|e| format!("Failed to create {}: {}", dir.display(), e))?;
            }
            std::fs::write(&path, plist_for(&exe.to_string_lossy()))
                .map_err(|e| format!("Failed to write {}: {}", path.display(), e))?;
        } else {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("Failed to remove {}: {}", path.display(), e)),
            }
        }
        Ok(())
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod imp {
    pub fn set_autorun(_enable: bool) -> Result<(), String> {
        Err("Start at login is not supported on this platform".to_string())
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    #[test]
    fn launch_agent_plist_runs_the_exe_at_login_and_escapes_the_path() {
        let p = super::imp::plist_for("/Applications/Patch & Bay.app/Contents/MacOS/patchbay");
        assert!(p.contains("<string>dev.patchbay.app</string>"));
        assert!(p.contains("<key>RunAtLoad</key>\n    <true/>"));
        assert!(p.contains("/Applications/Patch &amp; Bay.app/Contents/MacOS/patchbay"));
        assert!(!p.contains("Patch & Bay"));
    }
}
