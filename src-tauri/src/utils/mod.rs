//! Shared utility modules.

pub mod autorun;
#[cfg(windows)]
pub mod dpapi;
#[cfg(target_os = "macos")]
pub mod keychain;
pub mod log;
pub mod request_log;
pub mod secret_store;
#[cfg(target_os = "macos")]
pub mod shell_env;
