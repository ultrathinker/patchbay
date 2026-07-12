# Architecture

Patchbay is a Windows system-tray daemon that runs a local MCP gateway. This
document describes the stack, the module layout, and the key design decisions.

## Stack

- **Rust** (edition 2021), stable toolchain.
- **Tauri v2** — tray icon and Windows integration. No webview window
  (`"windows": []` in `tauri.conf.json`).
- **axum + tokio** — the HTTP gateway and async runtime.
- **parking_lot** — non-poisoning locks (a panic in one guard does not poison
  the lock for everyone else).
- **flexi_logger** + **log** — the always-on diagnostic log.
- **Windows API** (`windows` crate) — DPAPI (secrets), Job Objects (zero-orphan
  child kill), `MessageBoxW` (approval/confirm dialogs), the HKCU Run key
  (autostart).

## Module layout (`src-tauri/src`)

```
main.rs                 Entry point: logging bootstrap, panic hook, tray app setup
tray.rs                 System-tray menu: jacks, Settings, Custom/Forbidden, handlers
app_state.rs            Shared AppState: jack lifecycle, toggle pipeline, config save
approval.rs             First-connection Allow/Deny gate (blocking Win32 dialog)

config/
  mod.rs                Config load/save (save-then-commit discipline)
  schema.rs             patchbay.json schema (jacks, bays, seen_clients, overrides…)
  secrets.rs            DPAPI encryption of env/headers; idempotent re-save

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
  process.rs            Child spawn + Windows Job Object assignment

utils/
  autorun.rs            HKCU Run-key autostart
  dpapi.rs              Windows DPAPI encrypt/decrypt helpers
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
- **DPAPI at rest.** Secrets are encrypted with the running Windows user's
  DPAPI master key at save time; there is no separate password/master key.

## Configuration & data locations

| Path | Contents |
|---|---|
| `%APPDATA%\Patchbay\patchbay.json` | Config (jacks, port, autostart, seen agents, overrides, forbidden). Secrets stored DPAPI-encrypted (`dpapi:` prefix). |
| `%APPDATA%\Patchbay\patchbay.example.jsonc` | Fully-commented reference, written on first run. |
| `%APPDATA%\Patchbay\logs\patchbay_r<N>.log` | Level-1 diagnostic log (rotated, ~5 generations). |
| `%APPDATA%\Patchbay\logs\requests\<YYYY-MM-DD>.log` | Level-2 request/event log (opt-in). |

## Release profile

`[profile.release]` is tuned for a small, fast-starting daemon:
`opt-level = "z"`, `lto = true`, `codegen-units = 1`, `strip = true`,
`panic = "abort"`.
