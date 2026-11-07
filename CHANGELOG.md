# Changelog

All notable changes to Patchbay are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.4.0] — 2026-11-07

Patchbay now runs on macOS as well as Windows. The gateway, the jacks, the
permissions and both interfaces behave the same on the two systems; what had to
change is everything that touched the operating system — where secrets are kept,
how child processes are cleaned up, how the app starts at login, and how
dialogs are shown. Windows is unchanged (NSIS installer / exe, DPAPI, Job
Object, Run key).

### Added
- **macOS 11+ support, Apple silicon and Intel.** One universal build, shipped
  as a signed and notarized `Patchbay_1.4.0_universal.dmg` — open it and drag
  `Patchbay.app` to Applications. On macOS Patchbay is a menu-bar app: a
  monochrome template icon that follows light and dark mode, and no Dock icon.
  The tray menu and the popover window work the same as on Windows; the window
  uses the system WKWebView, so there is no WebView2 to install.
- **Secrets are encrypted on macOS too, with a key in your login Keychain.**
  DPAPI does not exist on macOS, so without this the tokens and env vars in
  `patchbay.json` would sit there as plaintext. A random 256-bit key is kept in
  the login Keychain (generic password, service `dev.patchbay.app`, account
  `config-encryption-key`) and each secret is sealed with AES-256-GCM under it,
  stored as `keychain:<base64(nonce||ciphertext)>`. The rules match Windows:
  type plaintext and it is encrypted on the next save, already-encrypted values
  (either prefix) are never re-encrypted, and a copied config or backup is
  useless without the Keychain. A value encrypted on the *other* OS (a config
  copied between machines) cannot be opened — it decrypts to empty and the log
  says to re-enter it. The key is created only when the Keychain item does not
  exist; if Keychain access fails, Patchbay never makes a new key, because that
  would orphan every secret already stored.
- **Start at login on macOS.** The tray item is called "Start at login" there
  (still "Start with Windows" on Windows). It writes a LaunchAgent,
  `~/Library/LaunchAgents/dev.patchbay.app.plist`, which takes effect from the
  next login and shows up in System Settings > General > Login Items. Turning
  it off removes the plist.
- **No orphaned MCP servers on macOS.** Windows has a Job Object that makes the
  OS kill every child when Patchbay goes away; macOS has no such thing, so each
  stdio child now leads its own process group. Switching a jack off kills the
  whole group (for example `npx` and the `node` it started), and quitting
  Patchbay — tray Quit, window Quit, logging out, or SIGTERM / SIGINT /
  SIGHUP — kills every group. The limit: if Patchbay itself is force-killed
  (SIGKILL) or crashes, macOS gives no OS-level guarantee; the children see EOF
  on stdin, which ends a well-behaved MCP server.
- **Native approval and delete-agent dialogs on macOS.** An AppleScript
  `display dialog` run through `/usr/bin/osascript`, in place of the Windows
  `MessageBox`. Delete defaults to Cancel, so a stray Return removes nothing.
  The agent name shown in the dialog is untrusted input, so it is escaped for
  AppleScript before it is placed in the script.
- **Your terminal's `PATH` on macOS.** An app started from Finder or at login
  gets a minimal `PATH`, so a jack whose `command` is `npx`, `node`, `uvx` or
  `docker` would fail to start even though it works in a terminal. At startup
  Patchbay now asks your login shell (`$SHELL -l -i -c`, 5 s timeout) for its
  `PATH` and adds `/opt/homebrew/bin`, `/opt/homebrew/sbin`, `/usr/local/bin`,
  `~/.local/bin`, `~/.cargo/bin`, `~/.bun/bin` and `~/.volta/bin`.
- **Launching Patchbay again opens the window (both platforms).** Starting it
  while it is already running (Start menu, Finder, Spotlight; on macOS also the
  system "reopen") used to do nothing visible. It now opens the popover window at
  the tray icon, so there is always a way back to the app — including when the
  menu bar has no room for its icon. "Show window" also works before the icon
  was ever clicked.
- CI builds and tests on both `windows-latest` and `macos-latest`.

### Changed
- **Where things live on macOS.** Config, state and logs are in
  `~/Library/Application Support/Patchbay/` (`patchbay.json`,
  `patchbay.state.json`, `patchbay.example.jsonc`, `logs/`) instead of
  `%APPDATA%\Patchbay\`, which stays as it was on Windows.
- **"Open config file" and "Open logs folder"** use the default text editor
  (`open -t`) and Finder on macOS; Notepad and Explorer on Windows, as before.
- **Secret handling goes through a platform-neutral layer**
  (`utils/secret_store.rs`) that picks DPAPI or the Keychain and recognises both
  `dpapi:` and `keychain:` as "already encrypted". The Windows format on disk is
  unchanged.
- **Building from source.** The Tauri CLI is run through npx, so `cargo tauri`
  no longer needs to be installed:
  `npx @tauri-apps/cli@2 build` for the Windows NSIS installer, and
  `npx @tauri-apps/cli@2 build --bundles app` for the macOS app bundle (add
  `--target universal-apple-darwin` for the universal build).
- **Documentation.** The README's "Windows-only" note is replaced by a
  "Platforms" note and the platform badge now reads Windows | macOS; README,
  `ABOUT.txt`, `docs/architecture.md`, `docs/security.md` and the commented
  example config describe both platforms.

### Fixed
- **Starting Patchbay a second time hid the running copy's log.** The second
  process started its logger before it noticed Patchbay was already running, and
  that rotated `patchbay_rCURRENT.log` away: the running copy went on writing to
  the renamed file, so "Open logs folder" showed a one-line log. The logger now
  starts only in the copy that keeps running. (Launching again is now how you
  open the window, so this would have happened often.)
- `ABOUT.txt` (the tray "About" text) said the popover window closes when you
  click away from it. That stopped being true in 1.3.4; it now lists the ways it
  does close — the X button, the tray icon again, or Esc.

## [1.3.7] — 2026-10-31

### Fixed
- **"Reload" in the window looked like it did nothing.** It restarted the
  servers but never told the window, so the statuses, tool counts and the
  config-file warning stayed as they were; the button also gave no sign that
  anything was happening, and every extra press started another full restart of
  every server on top of the first. Now the window gets the new state when the
  reload finishes, the button reads "Reloading..." and ignores presses until it
  is done, and a note reports the result ("Reloaded. 8 servers running.", or
  which server did not come back, or why `patchbay.json` could not be read).
  A reload also brings the gateway back up when it had failed to bind, as
  "Retry gateway" does.
- **The window could appear far too small, then jump when resized.** The window
  is created once and only hidden afterwards, so its size and its minimum
  stayed as they were when the display scaling, the resolution or the monitor
  changed (a projector, a dock, a different scaling). The old size could end up
  below the minimum for the new scaling, and the first drag on an edge made the
  system snap it up to the minimum in one jump. Every time it is shown, the
  window is now fitted to the monitor it appears on: converted to the new
  scaling, raised to that monitor's minimum, and cut down to the work area. The
  same fit runs when the scaling changes while it is open.

## [1.3.6] — 2026-09-13

### Fixed
- **No button refreshed a server's tool list.** Patchbay reads each server's
  tools once, when it connects. A server restarted with new tools (seen with a
  session-less HTTP server, which gives no sign of the restart) kept its old
  list in Patchbay indefinitely, and nothing in the UI could fix it: "Reload
  config" skipped every server already running, and switching a server off and
  on did not reconnect it when an agent's Custom list still used it (the
  connection was correctly kept alive for that agent, so "on" found it already
  running and did nothing). Now "Reload config" reconnects every server that
  should run, and switching a server ON — globally or in an agent's Custom
  list — always reconnects it and re-reads its tools. Switching OFF still
  keeps a connection that a Custom agent needs.

## [1.3.5] — 2026-09-05

### Added
- **The popover can be dragged by its header.** It used to stay pinned near
  the tray icon with no way to move it. The dot, title, and empty header
  space are now a native drag handle (Tauri's `data-tauri-drag-region`); the
  URL and close buttons still work as plain clicks and don't start a drag.

## [1.3.4] — 2026-09-05

### Fixed
- **The popover closed the instant you tried to resize it.** It hid itself on
  losing focus — ordinary flyout behaviour — and grabbing its own (invisible,
  frameless) resize border to drag an edge counted as a click outside the
  window, so it closed under the user's hand before a drag could start. It no
  longer hides on losing focus, or on any click landing elsewhere. There are
  now exactly two ways to close it: the tray icon again, or the ✕ button added
  in 1.3.3.
## [1.3.3] — 2026-09-05

The popover window: a close button, a resizable window, and a footer that no
longer wraps.

### Added
- **Close button** (top right of the popover header). Hides the window,
  same as clicking away or the tray icon — Patchbay keeps running.
- **The popover is now resizable.** Drag any edge. Width has a floor — the
  footer's six links (Jacks / Agents N / Settings / Logs / Reload / Quit)
  used to wrap onto a second line once the agent count grew a digit, and the
  floor is sized with margin above what that row needs, so shrinking the
  window can never bring the wrap back. Height can shrink to about three
  server rows before the list starts scrolling — short is a legitimate
  choice, not a broken one. Neither axis has an upper limit.

### Fixed
- **The footer wrapping onto a second line.** Six links in one row have no
  slack at all: at the old fixed width, `flex-shrink` on each link squeezed
  the row until a multi-word label (`Agents 6`) wrapped inside its own
  button instead of the row wrapping as a whole — which read as a broken
  layout, not a narrow window. Fixed at the CSS level (`flex-wrap: nowrap`
  on the footer, `white-space: nowrap` + `flex: 0 0 auto` on each link) so
  it cannot recur even if a future label runs longer still.
- **Repositioning after a resize.** Showing the popover always recomputed
  its position from the built-in default size, never the window's actual
  current one. Once resizing was possible those could disagree, silently
  drifting the window away from the tray icon by exactly the size
  difference on the next open. It now reads the real size back
  (`WebviewWindow::inner_size`) before positioning.
## [1.3.2] — 2026-09-03

Dependency housekeeping only — no behaviour change.

### Changed
- **19 duplicate crates dropped from the lockfile.** `winreg`, `dirs` and
  `windows` were pinned a major version behind what `tauri` itself depends on,
  so Cargo.lock carried both copies of each — two `windows` trees, two `dirs`,
  two `winreg`. Adopting Tauri's versions removes every duplicate. `windows`
  0.58 → 0.61 needed two call-site changes: `Win32::Foundation::BOOL` no longer
  exists (`OpenProcess` takes a plain `bool` for `bInheritHandle`, still
  `false`), and `LocalFree` now takes `Option<HLOCAL>`.
- CI moved to `actions/checkout@v7` and `actions/cache@v6`.
- Dependabot now ignores the GTK/WebKitGTK stack. Those crates are in
  Cargo.lock only because Cargo records every platform; they belong to Tauri's
  **Linux** backend, which a Windows-only app never compiles, and their versions
  are pinned by `tauri` → `gtk` 0.18 regardless. The one open advisory against
  them (unsoundness in `glib::VariantStrIter`) is unreachable here for the same
  reason.

## [1.3.1] — 2026-09-03

Five defects found by a full-application review, three of them reproducible on
a real install: a config that could destroy itself, permission lists that failed
open, and an admin surface any local process could use to switch a server back
on.

### Changed
- **`patchbay.json` no longer holds the list of agents that have connected.**
  That list is something Patchbay *observed*, not something the user
  *configured*, and it was the one field rewritten by ordinary agent traffic —
  an agent connecting appended to it, and `last_seen` was refreshed roughly once
  a minute per active agent. Every one of those writes was a chance to lose the
  whole configuration for a reason unrelated to configuration. It now lives in a
  sibling `patchbay.state.json`, which holds nothing secret and is disposable:
  delete it and previously-known agents simply look new. An existing install
  migrates itself on load — the list is still read from the old file, and the
  key disappears on the next save.
- **Agents may switch a server off, never on.** `patchbay__toggle_jack`,
  `patchbay__add_jack` and the identity-free `/admin/jacks*` REST routes let any
  caller on `127.0.0.1` switch a server the user had switched off back on —
  including a production database server — overriding the Custom lists, the
  forbidden gate and the approval dialog alike. One rule now covers all four
  entry points: nothing arriving over the gateway may make a server reachable
  that was not reachable already. Switching off stays allowed (that is how an
  agent drops a server it does not need); `add_jack` is allowed only with
  `patched: false`, including when the field is omitted, since it defaults to
  true. Refusals name the tray icon and the window so the model asks the user
  instead of retrying, and the advertised tool schemas state the rule up front.
  `remove_jack` is unchanged — it destroys a definition rather than granting
  access, so it sits outside this rule.

### Fixed
- **A corrupt `patchbay.json` no longer destroys itself.** When the file failed
  to parse at startup, Patchbay correctly kept an empty in-memory default and
  refused to save it — but only in `main`. Every mutator still wrote, and the
  mutators are not all user-initiated: one agent connecting reaches
  `record_seen_client`, and `last_seen` is refreshed about once a minute per
  active agent. So a config that failed to parse had roughly a minute to live
  before the empty default was written over it, taking every server definition
  and every DPAPI-encrypted secret with it. All writes now go through one
  guarded helper that refuses while the parse error stands (the tray shows it;
  "Reload config" clears it), and a source-level test keeps new mutators from
  walking around the guard.
- **Saving keeps one generation.** `patchbay.json` is rewritten by ordinary
  background activity and had no backup at all; the previous file is now copied
  to `patchbay.json.bak` before each atomic replace.

### Security
- **A Custom agent's permission list now fails closed.** `effective_patched`
  fell back to the *global* flag for any server the agent's Custom list did not
  name, so a gap in the list read as a grant — and a gap is exactly what drift
  produces. Found on a real install, where three agents in Custom mode had
  reachable access to a server nobody had granted them: their lists still named
  two jacks from an older config and named the current one nowhere, so every
  lookup missed and every miss was a grant. An enabled Custom list is now
  exhaustive — a server it does not name is denied.
- **Drifted permission maps are repaired on load.** Every loader now re-keys
  each Custom list against the servers that actually exist: entries naming
  servers that are gone are dropped, and servers the list never learned about
  are added as OFF. Deliberately unlike adding a server by hand, which
  propagates the value you chose: a repair of an unknown history must not hand
  out access nobody granted. **On first start after upgrading, a Custom agent
  may lose access to a server it had been reaching by accident; re-enable it on
  that agent's screen if it was intended.**

## [1.3.0] — 2026-09-03

Stage S13, part 1 (W0): the groundwork for an optional window UI alongside the
native tray menu. No user-visible interface change yet — `ui_mode` defaults to
`tray`, which behaves exactly as before.

### Added
- **An optional popover window**, alongside the native tray menu and selected by
  the new `ui_mode` setting (`tray` | `window` | `both`, default `tray` — an
  existing install upgrades with no visible change). Three screens: servers with
  live state and inline failures, agents with filters/multi-select/undo and
  per-agent permissions, and settings. Created lazily, so `tray` users never
  start a WebView2 process; if the webview cannot be created, Patchbay falls
  back to the tray menu for that session without rewriting the setting.
- **Settings → Interface** in the tray menu, so the window can be discovered and
  enabled without hand-editing `patchbay.json`.
- `ui_mode` config field. Parsed leniently: an absent, unknown or wrongly-typed
  value degrades to `tray` with a log line instead of failing the whole config
  parse.
- `SeenClient.last_seen` — an RFC3339 timestamp refreshed on the `initialize`
  path for clients that are already known, throttled to one persist per client
  per 60 s. Until now nothing recorded that an identity was still in use, which
  is what made long-dead one-shot script identities indistinguishable from the
  agents actually in daily use. Absent on pre-1.3.0 configs (rendered as "never
  seen since"); no backfill, because inventing activity that never happened
  would defeat the point.

### Security
- Client identities (`clientInfo.name` and the `X-Patchbay-Client` header) are
  now sanitized where they are resolved, before anything stores or displays
  them. Neither is authenticated, and in the tray menu that was inert — Win32
  treats a menu label as text. A webview does not: an identity containing markup
  would have become script running inside the process that holds your MCP
  credentials. Control characters are dropped (an embedded newline could forge a
  log line) and other disallowed characters are replaced rather than deleted, so
  tampering stays visible instead of turning `<script>` into `script`. The
  frontend additionally uses no raw-HTML sink, the popover's Tauri capability
  grants it only the event channel, and the strict CSP is unchanged — all four
  checked by tests. See `docs/security.md`.
- The window's state snapshot carries no secrets: no `env` values, no headers,
  no URL credentials, not even encrypted ones. Secrets travel one way only, into
  `add_jack`. A test asserts a known secret cannot appear in a serialized
  snapshot.

### Fixed
- `set_patched` now follows the same save-then-commit discipline as every other
  mutator. It used to write the new `patched` flag into the live config first
  and merely log a failed `config::save`, so a failed persist left the running
  process and the config file disagreeing — silently, on the most frequent
  operation in the app. A failed save is now reported back to the caller
  (`ToggleResult.status`) and nothing is committed.

## [1.2.13] — 2026-08-06

### Fixed
- The 1.2.12 fix widened the status-code gate to the whole 4xx range, but
  still required `is_session_invalid_body` to match the response body before
  treating a 4xx as recoverable. Live-diagnosed: bee-memory-bank's upstream
  (Apache) answers a stale `Mcp-Session-Id` with a bare "404 Not Found" HTML
  page — no JSON, no mention of "session" anywhere — so the body check could
  never match it, and every call failed with a raw "upstream HTTP 404"
  forever. Since HTTP 404 is the MCP spec's own recommended code for "session
  not found" (already the exact signal used by eUnifyMCP-Test/-Prod), a bare
  404 is now trusted unconditionally, with no body inspection required — any
  other 4xx still needs `is_session_invalid_body` to opt in. Verified live:
  bee-memory-bank recovered immediately after the fix.

## [1.2.12] — 2026-07-12

### Fixed
- The 1.2.11 fix widened WHICH error bodies count as "session invalid", but
  `send_once` only ever inspected the body when the upstream's HTTP status was
  exactly `400 Bad Request` — so the whole auto-recovery path stayed
  unreachable for any server that replies `404` instead (tabduct happens to
  use 400; eUnifyMCP-Test/-Prod use 404, the code the MCP spec itself
  recommends). Live-diagnosed: both eUnifyMCP jacks lost their upstream
  session mid-run and never self-healed, surfacing a raw "upstream HTTP 404"
  on every call. Broadened the status check from `== BAD_REQUEST` to
  `status.is_client_error()` (the whole 4xx range) — verified live, both
  jacks recovered immediately after the fix.

## [1.2.11] — 2026-07-12

### Fixed
- Broadened the 1.2.8 stale-upstream-session auto-recovery beyond tabduct's
  exact error signature: `is_session_invalid_body` only matched JSON-RPC error
  code `-32000` or the text "initialize first"/"no valid". Live-diagnosed:
  eUnifyMCP-Test uses a different code (`-32001`) and message ("Session not
  found"), so it never auto-recovered — every call just failed until the jack
  was manually toggled. Now matches any code in `-32000..=-32099` (the JSON-RPC
  spec's reserved "Server error" range) combined with a message mentioning
  "session", plus a widened text-only fallback ("not found"/"invalid"/
  "expired", not just the original two phrasings).

## [1.2.10] — 2026-07-12

### Fixed
- Reverted the 1.2.9 dead-session behavior for an **expired/unresolvable**
  session id: it now returns the spec-correct HTTP 404 again for every
  method, including `tools/call` and `tools/list`. Live-testing against a
  real Claude Code session showed the 1.2.9 "200 + model-readable text"
  disguise actively prevented a spec-compliant client's own transport-level
  auto-reconnect from firing — a real 404 lets it silently reinitialize and
  retry with zero visible disruption. The model-readable-text trick is kept
  only for a client that never sent a session id at all (no established
  session for a compliant client to recover from there).

## [1.2.9] — 2026-07-12

### Added
- Extended the dead/missing-session model-readable-text trick (see 1.2.7)
  from `tools/call` to `tools/list` too, via a synthetic
  `patchbay__session_expired` tool — reverted for the expired-session case
  in 1.2.10 (see above); see that entry for why.

## [1.2.8] — 2026-07-12

### Added
- **Auto-recover HTTP jacks from a stale upstream session.** A
  streamable-HTTP jack (e.g. tabduct) that had its `Mcp-Session-Id` forgotten
  by the upstream (typically after the upstream itself restarts) used to fail
  every call forever until manually toggled off and on. `HttpClient` now
  detects the upstream's "session invalid" response and transparently
  reinitializes before retrying once, deduped across concurrent callers.

## [1.2.7] — 2026-07-12

First versioned public baseline. This consolidates the initial implementation
with the post-ship feature work (per-agent identity/permissions, approval gate,
two-tier logging) and the subsequent security hardening.

### Added
- **Local MCP gateway.** Hand-rolled JSON-RPC/MCP server over Streamable HTTP on
  `http://127.0.0.1:39100/mcp` (axum + tokio), bound strictly to `127.0.0.1`,
  serving every connected coding agent from one URL with no per-agent config
  edits after a one-time wiring.
- **Jacks.** Each upstream MCP server is a "jack" (stdio child process or
  streamable-http endpoint); its tools are exposed to agents as
  `<jack>__<tool>`. One shared stdio child serves all agents.
- **Live toggle.** A tray checkbox per jack flips it patched/unpatched; agents
  that honor `tools/list_changed` pick up the change instantly, no restart.
- **DPAPI secret encryption.** API keys, bearer tokens and sensitive env vars in
  `patchbay.json` are encrypted at rest, tied to the Windows user account, at
  save time. No plaintext secrets on disk; no master password.
- **Per-agent identity.** Agents are identified by self-reported `clientInfo.name`
  or, preferably, by an `X-Patchbay-Client` header you set (overrides the
  self-reported name and stays stable across agent updates).
- **Per-agent "Custom" permissions.** Any seen agent can be given its own
  independent on/off list, seeded from the global snapshot and edited separately.
- **Connect-approval gate.** Optional one-time Allow/Deny Windows dialog the
  first time a brand-new identity connects (off by default).
- **Forbidden list.** Deny an agent identity entirely; symmetric allow/deny
  toggle in the tray.
- **Delete an agent.** Permanently purge an identity from memory (seen list,
  Custom permissions, Forbidden) with a confirmation dialog.
- **Built-in management tools.** `patchbay__add_jack`, `patchbay__remove_jack`,
  `patchbay__list_jacks`, `patchbay__toggle_jack` let a connected agent manage
  Patchbay without editing the config file.
- **Two-tier logging.** Level-1 always-on diagnostic log (size-based rotation
  via `flexi_logger`, panic hook records crashes). Level-2 opt-in request/event
  log (per-day files, size/count caps, sensitive header values and JSON keys
  redacted, long bodies truncated), toggled via the tray.
- **Live port rebind.** Changing `port` in `patchbay.json` + "Reload config"
  rebinds the gateway listener without a full app restart.
- **Honest error reporting.** Calls against dead/stale sessions return a
  model-readable instruction ("re-run initialize" / "wait for approval") instead
  of a raw transport error; calls to unpatched tools return a clear
  "server is UNPATCHED" message instead of a mysterious failure.
- **Windows integration.** System-tray menu (Tauri v2), autostart via the HKCU
  Run key, spawned children attached to a Windows Job Object (no orphans on
  quit), "Copy gateway URL", "Open config file", "Open logs folder".
- Compact tray popup with a Settings submenu; "About" at the top level showing
  the running version.

### Security
- Approval gate now **fails closed**: an unidentified client (no
  `X-Patchbay-Client` header and no `clientInfo.name`) cannot bypass the gate and
  is rejected at `initialize` before any session is minted.
- `apply_approval_decision` fails closed on a `Deny` whose config save fails —
  the forbidden entry is committed in memory regardless, so a just-denied
  identity cannot complete its session with access.
- Broadened request-log redaction: sensitive-header detection now covers
  `Cookie`/`Set-Cookie`/`Secret`/`Auth`/`Password` (was only
  `Authorization`/`token`/`key`), and a new `redact_body()` recursively redacts
  sensitive JSON keys inside logged request params (e.g. a jack's `headers`/`env`
  passed to `patchbay__add_jack`), which header redaction alone could not cover.

[Unreleased]: https://github.com/ultrathinker/patchbay/compare/v1.4.0...HEAD
[1.4.0]: https://github.com/ultrathinker/patchbay/releases/tag/v1.4.0
[1.2.13]: https://github.com/ultrathinker/patchbay/releases/tag/v1.2.13
[1.2.12]: https://github.com/ultrathinker/patchbay/releases/tag/v1.2.12
[1.2.11]: https://github.com/ultrathinker/patchbay/releases/tag/v1.2.11
[1.2.10]: https://github.com/ultrathinker/patchbay/releases/tag/v1.2.10
[1.2.9]: https://github.com/ultrathinker/patchbay/releases/tag/v1.2.9
[1.2.8]: https://github.com/ultrathinker/patchbay/releases/tag/v1.2.8
[1.2.7]: https://github.com/ultrathinker/patchbay/releases/tag/v1.2.7
