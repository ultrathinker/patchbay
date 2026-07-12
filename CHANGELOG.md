# Changelog

All notable changes to Patchbay are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/ultrathinker/patchbay/compare/v1.2.7...HEAD
[1.2.7]: https://github.com/ultrathinker/patchbay/releases/tag/v1.2.7
