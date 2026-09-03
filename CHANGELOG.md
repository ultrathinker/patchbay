# Changelog

All notable changes to Patchbay are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/ultrathinker/patchbay/compare/v1.2.13...HEAD
[1.2.13]: https://github.com/ultrathinker/patchbay/releases/tag/v1.2.13
[1.2.12]: https://github.com/ultrathinker/patchbay/releases/tag/v1.2.12
[1.2.11]: https://github.com/ultrathinker/patchbay/releases/tag/v1.2.11
[1.2.10]: https://github.com/ultrathinker/patchbay/releases/tag/v1.2.10
[1.2.9]: https://github.com/ultrathinker/patchbay/releases/tag/v1.2.9
[1.2.8]: https://github.com/ultrathinker/patchbay/releases/tag/v1.2.8
[1.2.7]: https://github.com/ultrathinker/patchbay/releases/tag/v1.2.7
