# Patchbay

> One tray toggle for **all** your AI coding agents' MCP servers — at once, no
> restarts.

![CI](https://github.com/ultrathinker/Patchbay/actions/workflows/ci.yml/badge.svg)
![Rust](https://img.shields.io/badge/rust-stable-orange)
![Tauri](https://img.shields.io/badge/Tauri-v2-blue)
![Platform](https://img.shields.io/badge/platform-Windows-blue)

Patchbay is a Windows system-tray app + a local **MCP gateway** that lets you
toggle MCP servers on/off from the tray — **live, for ALL your CLI coding agents
at once** (Claude Code, Codex, Antigravity, Kilo), with no agent restart and no
per-agent config edits after a one-time wiring.

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
- **Approval gate.** Optionally require a one-time Allow/Deny Windows dialog the
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
  DPAPI-encrypted (tied to your Windows user account) the moment they're saved —
  no master password, no plaintext on disk.
- **Two-tier logging.** An always-on diagnostic log, plus an opt-in request/event
  log that records every MCP request and admin action with secrets redacted.
- **Local only.** Bound strictly to `127.0.0.1`; no webview window, no network
  exposure. Spawned child processes are tied to a Windows Job Object so quitting
  Patchbay can never leave orphans behind.

> **Windows-only.** Patchbay uses Windows DPAPI, the registry (autostart), Job
> Objects and the tray, so it targets Windows. The gateway protocol itself is
> cross-platform MCP, but the host side is not.

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

Edit the config file at `%APPDATA%\Patchbay\patchbay.json`. A fully commented
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

- **Secrets**: type them in **plaintext**; Patchbay DPAPI-encrypts them to a
  `dpapi:`-prefixed value on the next save, so only your Windows user account
  can ever decrypt them. The config on disk never keeps plaintext secrets.
- **`prod` ships unpatched** (`patched: false`) — opt in explicitly.
- **Windows note**: a `command` like `npx` or `npm` works out of the box —
  Patchbay runs bare commands through `cmd /C` so the `.cmd` launchers npm
  installs on Windows resolve correctly.

Each jack's tools are exposed to agents as `<jack>__<tool>`
(e.g. `github__create_issue`).

## Tray menu

- One **checkable row per jack** — tick = patched (live), untick = unpatched
  (off, instantly). Toggling is reflected across every connected agent with no
  restart.
- **Retry gateway** — re-attempts binding the local port (use this if the tooltip
  shows `GATEWAY FAILED`, e.g. because port 39100 was in use at startup and has
  since freed up).
- **Reload config** — re-reads `patchbay.json` and reconciles jacks.
- **Open config file** / **Copy gateway URL** — convenience helpers.
- **Start with Windows** — toggles autostart.
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

## 4. Build from source

This project builds under the **Visual Studio 2022 BuildTools** environment
(`vcvars64.bat`); the included `_build.ps1` captures that environment and runs
the release build. A normal `cargo build` from a **VS Developer Command Prompt**
also works.

```bash
# Debug build + tests (from src-tauri)
cargo build
cargo test

# Optimized release binary (Rust toolchain only)
cargo build --release

# Windows NSIS installer (needs the Tauri prerequisites)
cargo tauri build
```

Targets: release **binary < 12 MB**, **idle RAM < 40 MB** (currently ~3 MB
binary / ~16 MB idle RAM). Bound strictly to `127.0.0.1`; no webview window in
v0.1.

## Architecture

```
                ┌─────────────────────────────────────────────┐
 Claude Code ──┤                                             │
 Codex ────────┤   Patchbay gateway (axum + tokio)            ├──► stdio jack (child proc)
 Antigravity ──┤   http://127.0.0.1:39100/mcp                 │     e.g. npx some-mcp
 Kilo ─────────┤   - per-agent session + notification stream  │
                │   - tools exposed as <jack>__<tool>         ├──► streamable-http jack
                │   - DPAPI-encrypted secrets                 │     e.g. https://host/mcp
                └──────────────────────┬──────────────────────┘
                                       │ Windows tray (Tauri v2)
                                       ▼
                          toggle jacks / per-agent perms
                          config: %APPDATA%\Patchbay\patchbay.json
```

Patchbay speaks the [Model Context Protocol](https://modelcontextprotocol.io)
over Streamable HTTP. The gateway is hand-rolled (JSON-RPC on axum/tokio — no
generic SDK) so it controls session tracking and live `tools/list_changed`
notifications directly. See [`docs/architecture.md`](docs/architecture.md) and
the user-facing [`ABOUT.txt`](ABOUT.txt).

## Documentation

- [Architecture](docs/architecture.md) — modules, layers, key decisions.
- [Security model](docs/security.md) — trust boundary, DPAPI, localhost binding.
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
