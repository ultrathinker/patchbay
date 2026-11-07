# Architecture

Patchbay is a system-tray daemon (a menu-bar app on macOS) that runs a local MCP
gateway. It runs on Windows and macOS 11+ (Apple silicon and Intel, one
universal build). This document describes the stack, the module layout, and the
key design decisions; where the two platforms differ, both are described.

## Stack

- **Rust** (edition 2021), stable toolchain.
- **Tauri v2** — tray icon and OS integration. No webview window is
  created at startup (`"windows": []` in `tauri.conf.json`); since S13 an
  OPTIONAL popover window is built lazily at runtime when `ui_mode` is
  `window`/`both` (see `_planning/WINDOW_UI_PLAN.md`). The webview is WebView2
  on Windows and the system WKWebView on macOS.
- **axum + tokio** — the HTTP gateway and async runtime.
- **parking_lot** — non-poisoning locks (a panic in one guard does not poison
  the lock for everyone else).
- **flexi_logger** + **log** — the always-on diagnostic log.
- **Windows API** (`windows` crate, `winreg`) — DPAPI (secrets), Job Objects
  (zero-orphan child kill), `MessageBoxW` (approval/confirm dialogs), the HKCU
  Run key (autostart).
- **macOS APIs** (`security-framework`, `aes-gcm`, `libc`) — the login Keychain
  (holds the secret-encryption key; AES-256-GCM seals the values), process
  groups + `killpg` (zero-orphan child kill), `osascript` (approval/confirm
  dialogs), a LaunchAgent plist (autostart). The app runs as an "accessory"
  (menu-bar) app: no Dock icon.

## Module layout (`src-tauri/src`)

```
main.rs                 Entry point: logging bootstrap, panic hook, tray app setup,
                        single-instance / macOS "reopen" → show the window,
                        Unix signal handlers + exit hook that kill child processes
tray.rs                 System-tray menu: jacks, Settings, Custom/Forbidden, handlers
app_state.rs            Shared AppState: jack lifecycle, toggle pipeline, config save
approval.rs             First-connection Allow/Deny gate and delete-agent confirm
                        (blocking native dialog: MessageBoxW on Windows,
                        `osascript` display dialog on macOS)

config/
  mod.rs                Config load/save (save-then-commit discipline)
  schema.rs             patchbay.json schema (jacks, bays, seen_clients, overrides…)
  policy.rs             what a gateway caller may change (S14: off yes, on no)
  secrets.rs            Encryption of env/headers via utils/secret_store; idempotent re-save

gateway/
  http.rs               axum routes (/mcp, /admin/jacks), request logging hook
  jsonrpc.rs            Hand-rolled JSON-RPC framing (no rmcp)
  handlers.rs           tools/call dispatch; built-in patchbay__* meta-tools
  session.rs            Per-agent session tracking + Mcp-Session-Id enforcement
  sse.rs                Per-session SSE notification stream
  tools.rs              Tool-list assembly (merged upstream tools)

upstream/
  mod.rs                UpstreamClient trait + lifecycle; shared-child refcounting
  client.rs             Transport-agnostic client
  stdio.rs              stdio transport: spawn child, MCP over stdin/stdout
  http.rs               streamable-http transport: SSE-parsed upstream
  process.rs            Zero-orphan child handling: Windows Job Object
                        (KILL_ON_JOB_CLOSE); on macOS/Unix process groups with
                        `kill_tree` (one jack) and `kill_all` (quit)

utils/
  autorun.rs            Start at login: HKCU Run key (Windows) / LaunchAgent plist (macOS)
  secret_store.rs       Platform-neutral facade over the secret stores; knows the
                        `dpapi:` and `keychain:` prefixes, never re-encrypts
  dpapi.rs              Windows DPAPI encrypt/decrypt helpers (Windows only)
  keychain.rs           AES-256-GCM with a key in the login Keychain (macOS only)
  shell_env.rs          Imports the login shell's PATH at startup (macOS only)
  log.rs                flexi_logger wrapper + project log() helper + panic hook
  request_log.rs        Opt-in Level-2 request/event log with secret redaction
```

## Key decisions

- **Hand-rolled MCP/JSON-RPC, no `rmcp`.** The gateway speaks MCP directly on
  axum so it controls per-agent session tracking and live
  `tools/list_changed` notifications precisely.
- **Strictly `127.0.0.1`.** The listener binds loopback only; it is never
  network-reachable.
- **One shared stdio child per jack.** All agents share one upstream child
  process (e.g. one browser for a browser-automation MCP). A child stays alive
  while the global checkbox is on **or** any agent's Custom list opts it in.
- **Save-then-commit.** Config mutators persist to disk *before* committing the
  change to live state, so a failed write can't leave live state and disk out of
  sync.
- **Honest error results.** A call against a dead/unpatched server returns a
  `CallToolResult{is_error:true}` with a model-readable explanation rather than
  a raw transport error that some agents retry forever.
- **Secrets encrypted at rest, per platform.** Secrets are encrypted at save time
  by `utils/secret_store.rs`, which picks the platform's user-bound store and
  tags each value with a prefix saying how to open it. Windows: DPAPI with the
  running user's master key (`dpapi:<base64>`). macOS: AES-256-GCM under a
  random 256-bit key kept in the user's login Keychain (generic password,
  service `dev.patchbay.app`, account `config-encryption-key`), stored as
  `keychain:<base64(nonce||ciphertext)>`. There is no separate password or
  master key to manage on either. A value carrying either prefix is already
  encrypted and is never re-encrypted; a value from the *other* platform's
  store cannot be opened and decrypts to empty, with a log line asking the user
  to re-enter it. The macOS key is created only when the Keychain item does not
  exist — if Keychain access fails, Patchbay never generates a replacement,
  because that would orphan every secret sealed under the old key.
- **No orphans, per platform.** `upstream/process.rs` hides the difference
  behind `Job::assign_pid`, `kill_tree` and `kill_all`. On Windows the OS does
  it: a Job Object with `KILL_ON_JOB_CLOSE` kills the whole tree even if
  Patchbay crashes (`kill_tree`/`kill_all` are no-ops). On macOS each stdio
  child leads its own process group (`process_group(0)`), `kill_tree` kills the
  group when a jack is switched off, and `kill_all` kills every group on quit
  (`RunEvent::Exit`, which covers tray/window Quit and logout, plus the
  SIGTERM/SIGINT/SIGHUP handlers). The limit: there is no OS-level kill-on-crash
  on macOS, so a force-killed Patchbay leaves children to notice EOF on stdin.
- **A usable `PATH` on macOS.** An app started from Finder or at login gets
  launchd's minimal `PATH`, so `utils/shell_env.rs` runs the user's login shell
  (`$SHELL -l -i -c`, 5 s timeout) once at startup — before any thread exists —
  and adds the usual install directories (`/opt/homebrew/bin`,
  `/opt/homebrew/sbin`, `/usr/local/bin`, `~/.local/bin`, `~/.cargo/bin`,
  `~/.bun/bin`, `~/.volta/bin`).
- **Second launch shows the window.** Starting Patchbay again while it runs
  (single-instance callback on both platforms; the macOS "reopen" event) opens
  the popover at the tray icon through `tray::show_window_anywhere`.

## Configuration & data locations

`<config dir>` is `%APPDATA%\Patchbay\` on Windows and
`~/Library/Application Support/Patchbay/` on macOS.

| Path | Contents |
|---|---|
| `<config dir>/patchbay.json` | Config — what the **user** decided: jacks, port, autostart, per-agent overrides, forbidden agents. Secrets stored encrypted (`dpapi:` prefix on Windows, `keychain:` on macOS). Written only when a setting changes. |
| `<config dir>/patchbay.json.bak` | The previous generation, refreshed before every save (S14). |
| `<config dir>/patchbay.state.json` | Runtime state — what Patchbay **observed**: which agents have connected, and when. Rewritten by ordinary agent traffic; disposable (delete it and known agents look new). Split out in S14 so that traffic stops rewriting the file holding the secrets. |
| `<config dir>/patchbay.example.jsonc` | Fully-commented reference, written on first run. |
| `<config dir>/logs/patchbay_r<N>.log` | Level-1 diagnostic log (rotated, ~5 generations). |
| `<config dir>/logs/requests/<YYYY-MM-DD>.log` | Level-2 request/event log (opt-in). |

Outside `<config dir>`, "start at login" leaves one artifact: on Windows the
HKCU `Run` key value `Patchbay`; on macOS the LaunchAgent
`~/Library/LaunchAgents/dev.patchbay.app.plist` (it takes effect at the next
login and is removed when the setting is turned off).

## Release profile

`[profile.release]` is tuned for a small, fast-starting daemon:
`opt-level = "z"`, `lto = true`, `codegen-units = 1`, `strip = true`,
`panic = "abort"`.
