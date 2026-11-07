# Patchbay

> One tray toggle for **all** your AI coding agents' MCP servers — at once, no
> restarts.

![CI](https://github.com/ultrathinker/patchbay/actions/workflows/ci.yml/badge.svg)
![Rust](https://img.shields.io/badge/rust-stable-orange)
![Tauri](https://img.shields.io/badge/Tauri-v2-blue)
![Platform](https://img.shields.io/badge/platform-Windows%20%7C%20macOS-blue)

Patchbay is a system-tray app (a menu-bar app on macOS) + a local **MCP
gateway** that lets you toggle MCP servers on/off from the tray — **live, for
ALL your CLI coding agents at once** (Claude Code, Codex, Antigravity, Kilo),
with no agent restart and no per-agent config edits after a one-time wiring.

It speaks the [Model Context Protocol](https://modelcontextprotocol.io) over
Streamable HTTP on `http://127.0.0.1:39100/mcp` (bound strictly to localhost).
Point every agent at that single URL once; from then on you flip servers on and
off in the Patchbay tray and every connected agent picks it up instantly.

## Features

- **One URL for every agent.** Point Claude Code, Codex, Google Antigravity and
  Kilo at `http://127.0.0.1:39100/mcp` once — never edit their configs again.
- **Live toggle.** Tick/untick a server in the tray; its tools appear or
  disappear for every connected agent instantly (agents that honor
  `tools/list_changed`; Codex needs a session restart — its limitation, not
  Patchbay's).
- **Per-agent identity.** An agent is identified by its self-reported name or,
  better, by an `X-Patchbay-Client` header you set, so you can tell a personal
  and a work profile of the same tool apart.
- **Per-agent permissions.** Give any agent its own independent on/off list
  ("Custom permissions"), separate from the global checkboxes.
- **Approval gate.** Optionally require a one-time Allow/Deny system dialog the
  first time a brand-new identity connects (off by default).
- **Honest errors.** Calling an unpatched server returns a clear message to the
  agent instead of a mystery failure. A Patchbay restart invalidates every
  agent's MCP session (in-memory only, unavoidable) — a spec-compliant client
  (Claude Code included) silently reconnects on its very next tool call, no
  visible disruption; an agent that never called `initialize` at all gets an
  explicit instruction instead of a confusing error.
- **Manage Patchbay from an agent.** Built-in tools `patchbay__add_jack`,
  `patchbay__remove_jack`, `patchbay__list_jacks`, `patchbay__toggle_jack` let a
  connected agent add/remove/flip servers without you touching the config file.
- **Secrets encrypted at rest.** API keys, bearer tokens and env vars are
  encrypted the moment they're saved — with DPAPI (tied to your Windows user
  account) on Windows, or with a random key kept in your login Keychain on macOS
  — no master password, no plaintext on disk.
- **Two-tier logging.** An always-on diagnostic log, plus an opt-in request/event
  log that records every MCP request and admin action with secrets redacted.
- **Local only.** Bound strictly to `127.0.0.1`; no network exposure. The
  optional window UI (below) is a local webview that loads only embedded assets
  under a strict CSP. Spawned child processes never outlive a quit: on Windows
  they are tied to a Job Object (the OS kills them even if Patchbay crashes); on
  macOS each one runs in its own process group that Patchbay kills when you
  switch the jack off or quit.

> **Platforms.** Patchbay runs on **Windows** and on **macOS 11+** (Apple
> silicon and Intel — one universal build). On Windows it ships as an NSIS
> installer / exe. On macOS it ships as a signed and notarized
> `Patchbay_1.4.0_universal.dmg`: open it and drag `Patchbay.app` to
> Applications. On macOS Patchbay is a menu-bar app — a monochrome icon in the
> menu bar that follows light/dark mode, and no Dock icon. The tray menu and the
> popover window work the same on both. Where behaviour differs by OS, this
> document says so.

## Vocabulary

- **Jack** — one upstream MCP server registered in Patchbay (e.g. a GitHub MCP, a
  database MCP). Think of a patchbay's input socket.
- **Patched** — the jack is live: its tools are exposed through the gateway to
  every agent, prefixed with the jack name.
- **Unpatched** — the jack is off: its tools disappear from every agent
  immediately, and any call to one is rejected with a clear message.

## 1. One-time wiring (per agent — do this once)

After this, **never** edit agent configs again; toggle servers in Patchbay.

| Agent | How to point it at the gateway |
|---|---|
| **Claude Code** | `claude mcp add --transport http patchbay http://127.0.0.1:39100/mcp` |
| **Codex** | in `~/.codex/config.toml` →<br>`[mcp_servers.patchbay]`<br>`url = "http://127.0.0.1:39100/mcp"` |
| **Google Antigravity** | in `~/.gemini/config/mcp_config.json` → an entry with<br>`"serverUrl": "http://127.0.0.1:39100/mcp"` |
| **Kilo** | in its MCP config →<br>`{ "type": "streamable-http", "url": "http://127.0.0.1:39100/mcp" }` |

## 2. Configuring servers (jacks)

Edit the config file at `%APPDATA%\Patchbay\patchbay.json` on Windows, or
`~/Library/Application Support/Patchbay/patchbay.json` on macOS (the tray's
**Open config file** opens it for you). A fully commented
`patchbay.example.jsonc` is written next to it on first run for reference. Each
jack is either **stdio** (a local command) or **streamable-http** (a remote URL):

```jsonc
{
  "version": 1,
  "port": 39100,
  "autostart": false,
  "jacks": [
    {
      "name": "prod",
      "patched": false,
      "transport": "stdio",
      "command": "npx",
      "args": ["-y", "some-db-mcp"],
      "env": { "DB_TOKEN": "secret-token-here" },
      "sharing": "shared"
    },
    {
      "name": "docs",
      "patched": true,
      "transport": "streamable-http",
      "url": "https://example.com/mcp",
      "headers": { "Authorization": "Bearer xyz" }
    }
  ],
  "bays": {}
}
```

After editing, click **Reload config** in the tray.

- **Secrets**: type them in **plaintext**; Patchbay encrypts them on the next
  save, so the config on disk never keeps plaintext secrets. Values that are
  already encrypted are never re-encrypted.
  - **Windows**: DPAPI (CurrentUser scope), stored as `dpapi:<base64>` — only
    your Windows user account can decrypt them.
  - **macOS**: AES-256-GCM with a random 256-bit key kept in your login Keychain
    (generic password, service `dev.patchbay.app`, account
    `config-encryption-key`), stored as `keychain:<base64>`. A copied config or
    backup is useless without that Keychain item.
  - A value encrypted on the *other* OS (a config copied between machines)
    cannot be opened: it decrypts to empty and the log says to re-enter it.
- **`prod` ships unpatched** (`patched: false`) — opt in explicitly.
- **Windows note**: a `command` like `npx` or `npm` works out of the box —
  Patchbay runs bare commands through `cmd /C` so the `.cmd` launchers npm
  installs on Windows resolve correctly.
- **macOS note**: an app started from Finder or at login gets a minimal `PATH`,
  so at startup Patchbay asks your login shell (`$SHELL -l -i -c`, 5 s timeout)
  for its `PATH` and adds `/opt/homebrew/bin`, `/opt/homebrew/sbin`,
  `/usr/local/bin`, `~/.local/bin`, `~/.cargo/bin`, `~/.bun/bin` and
  `~/.volta/bin`. A `command` like `npx`, `node`, `uvx` or `docker` therefore
  resolves as it does in your terminal.

Each jack's tools are exposed to agents as `<jack>__<tool>`
(e.g. `github__create_issue`).

## Two interfaces — pick one

Since 1.3.0 Patchbay has **two** user interfaces over the same state, and a
setting chooses which one the tray icon drives.

| `ui_mode` | left-click the icon | right-click the icon |
|---|---|---|
| `tray` *(default)* | native menu | native menu |
| `window` | popover window | small menu: Show window / Reload config / Quit |
| `both` | popover window | native menu |

Switch it in **Settings → Interface** (tray) or on the **Settings** screen
(window). The default is `tray`, so upgrading changes nothing until you opt in.

**Why a window exists.** A tray menu closes the moment you click anything in
it. That is fine for a single blind toggle you already know the position of, and
poor for everything else: you cannot see whether a checkbox actually moved, a
server that takes a second to start has nowhere to report "starting…" or
"failed: connection refused", and flipping four servers means opening the menu
four times. The popover stays open, shows live state, and supports selection —
so deleting sixty accumulated agent identities is one filter, one select-all and
one click instead of sixty menu traversals. It is resizable by dragging any
edge — width has a floor so the bottom bar's six links never wrap, height can
shrink to about three server rows before the list scrolls, and there is no
upper limit on either.

**What the window shows.**

- **Jacks** — one row per server with a real switch, its transport, its live
  tool count, and any failure written on the row with a Retry next to it.
  Turning on a production-class server asks first, inline in the row.
- **Agents** — every identity that has connected, with a live dot for the ones
  connected right now, filters (including **Unused**: never seen since first
  contact, or quiet for 30 days), multi-select, and bulk delete or deny with an
  undo strip. Open one to give it its own per-server permissions.
- **Settings** — interface, gateway port, autostart, the approval gate, request
  logging, and the config/log folders.

The window is created the first time you open it, so `tray` users never pay for
it. On Windows it needs the WebView2 runtime (present on Windows 11 and current
Windows 10); on macOS it uses the system WKWebView, so there is nothing to
install. If it cannot start, Patchbay logs why and falls back to the tray menu
for that session without changing your setting.

**Launching Patchbay again opens the window.** Starting Patchbay while it is
already running (Start menu, Finder, Spotlight; on macOS also the system
"reopen") opens the popover window at the tray icon instead of doing nothing —
so there is always a way back to it, even when the icon is not visible (on
macOS, when the menu bar has no room for it).
**Show window** also works before the icon has ever been clicked.

## Tray menu

- One **checkable row per jack** — tick = patched (live), untick = unpatched
  (off, instantly). Toggling is reflected across every connected agent with no
  restart.
- **Retry gateway** — re-attempts binding the local port (use this if the tooltip
  shows `GATEWAY FAILED`, e.g. because port 39100 was in use at startup and has
  since freed up).
- **Reload config** — re-reads `patchbay.json` and reconciles jacks.
- **Open config file** / **Copy gateway URL** — convenience helpers. The config
  file opens in Notepad on Windows and in your default text editor on macOS.
- **Open logs folder** — opens the folder holding Patchbay's log files (in
  Explorer on Windows, Finder on macOS).
- **Enable request logging** (in the Settings submenu) — toggles a second-tier log that records each MCP request/response with secrets redacted; off by default.
- **Interface** (in the Settings submenu) — chooses between the tray menu, the
  popover window, or both. See above.
- **Start with Windows** (on macOS: **Start at login**) — toggles autostart. On
  Windows this sets the `Patchbay` value in the HKCU `Run` key. On macOS it
  writes a LaunchAgent, `~/Library/LaunchAgents/dev.patchbay.app.plist`, that
  runs the app at login; it takes effect from your next login and shows up in
  System Settings → General → Login Items. Turning it off removes the plist.
- **Quit**.

The tray tooltip shows the patched/total counts and the gateway URL, or a
distinct `Patchbay — GATEWAY FAILED: <reason>` when the port couldn't be bound.

## 3. Caveats (read once)

- **Codex does not honor `tools/list_changed`.** After you PATCH a server, Codex
  won't see the new tools until its session restarts; and if you UNPATCH, a stale
  Codex call returns a clear "server is UNPATCHED — ask the user to enable it"
  message instead of the tool. Claude Code, Gemini CLI and OpenCode update live.
- **Namespacing.** Tools are exposed as `<jack>__<tool>`
  (e.g. `github__create_issue`); any per-agent allow-rules referencing the old
  bare tool names must be updated **once**.
- **Shared stdio child.** One child process per stdio jack serves all agents —
  e.g. a browser-automation MCP shares one browser across Claude, Codex, etc.
- **claude.ai connectors** (Gmail / Calendar / Drive) are provided by claude.ai
  directly and cannot be routed through Patchbay.
- **Unpatch = no NEW reachability.** New calls are blocked instantly and a stdio
  child is killed; an already-in-flight remote HTTP request is best-effort.
- **macOS: no kill-on-crash guarantee.** Switching a jack off kills its whole
  process group (e.g. `npx` and the `node` it started), and quitting Patchbay —
  tray Quit, window Quit, logout, or SIGTERM / SIGINT / SIGHUP — kills
  every group. But if Patchbay itself is force-killed (SIGKILL) or crashes,
  macOS gives no OS-level guarantee (Windows does, via the Job Object); the
  children then see EOF on stdin, which ends a well-behaved MCP server.

## 4. Build from source

**Windows.** This project builds under the **Visual Studio 2022 BuildTools**
environment (`vcvars64.bat`); the included `_build.ps1` captures that
environment and runs the release build. A normal `cargo build` from a **VS
Developer Command Prompt** also works.

**macOS.** Install the Xcode command line tools (`xcode-select --install`) and
Rust; then `cargo build` / `cargo test` work from `src-tauri` as below.

```bash
# Debug build + tests (from src-tauri)
cargo build
cargo test

# Optimized release binary (Rust toolchain only)
cargo build --release

# Windows NSIS installer (needs the Tauri prerequisites)
npx @tauri-apps/cli@2 build

# macOS app bundle (Patchbay.app)
npx @tauri-apps/cli@2 build --bundles app

# macOS universal (Apple silicon + Intel) — first: rustup target add x86_64-apple-darwin
npx @tauri-apps/cli@2 build --bundles app --target universal-apple-darwin
```

The Tauri CLI is run through `npx`, so there is no need to install
`cargo tauri`. CI builds and tests on both `windows-latest` and `macos-latest`.

Targets: release **binary < 12 MB**, **idle RAM < 40 MB in tray mode**
(currently ~3 MB binary / ~16 MB idle RAM, measured on Windows). Window mode
adds a webview process (WebView2 on Windows, WKWebView on macOS) once the
popover has been opened at least once — the window is created lazily, so a user
who stays in tray mode pays nothing. Bound strictly to `127.0.0.1`.

## Architecture

```
                ┌─────────────────────────────────────────────┐
 Claude Code ──┤                                             │
 Codex ────────┤   Patchbay gateway (axum + tokio)            ├──► stdio jack (child proc)
 Antigravity ──┤   http://127.0.0.1:39100/mcp                 │     e.g. npx some-mcp
 Kilo ─────────┤   - per-agent session + notification stream  │
                │   - tools exposed as <jack>__<tool>         ├──► streamable-http jack
                │   - encrypted secrets (DPAPI / Keychain)    │     e.g. https://host/mcp
                └──────────────────────┬──────────────────────┘
                                       │ tray / menu bar (Tauri v2)
                                       ▼
                          toggle jacks / per-agent perms
                          config: <config dir>/patchbay.json
                          state:  <config dir>/patchbay.state.json
```

`<config dir>` is `%APPDATA%\Patchbay\` on Windows and
`~/Library/Application Support/Patchbay/` on macOS.

Patchbay speaks the [Model Context Protocol](https://modelcontextprotocol.io)
over Streamable HTTP. The gateway is hand-rolled (JSON-RPC on axum/tokio — no
generic SDK) so it controls session tracking and live `tools/list_changed`
notifications directly. See [`docs/architecture.md`](docs/architecture.md) and
the user-facing [`ABOUT.txt`](ABOUT.txt).

## Documentation

- [Architecture](docs/architecture.md) — modules, layers, key decisions.
- [Security model](docs/security.md) — trust boundary, DPAPI / Keychain secrets,
  localhost binding.
- [ABOUT.txt](ABOUT.txt) — full user-facing guide (every tray menu item).
- [CHANGELOG.md](CHANGELOG.md) — release history.

## Contributing

Issues and pull requests are welcome. Please read
[CONTRIBUTING.md](CONTRIBUTING.md) first, and report security issues privately
(see [SECURITY.md](SECURITY.md)) — **do not** open a public issue for a
vulnerability.

## Acknowledgements

Patchbay's tray/DPAPI/autostart/Job-Object integration patterns were adapted
from an earlier Tauri v2 system-tray app by the same author.

## License

[MIT](LICENSE) — see the file for details.
