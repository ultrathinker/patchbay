# Security model

This document expands on the trust boundary in [`SECURITY.md`](../SECURITY.md)
with the implementation details an auditor or contributor needs.

## Principle: local-only by construction

Patchbay binds its MCP gateway **strictly to `127.0.0.1:39100`**. It is not
network-reachable; reaching it requires running code on the local machine as the
same Windows user. This is the single most important property: most
"remote attacker" scenarios are out of scope by design.

## Secrets at rest — DPAPI

Sensitive values in `patchbay.json` are encrypted with Windows **DPAPI**, scoped
to the current user account (`CryptProtectData`):

- jack `env` map values,
- jack `headers` map values (e.g. `Authorization`).

On save, every plaintext sensitive value is encrypted and rewritten with a
`dpapi:` prefix; re-saving an already-prefixed value is a no-op (no double
encryption). At use time the values are decrypted in memory and passed to the
spawned child / HTTP request. **Plaintext secrets are never persisted to disk**
by Patchbay after the first save.

Consequence: a `patchbay.json` copied to another user account or machine cannot
be decrypted there. Back up the config together with the user profile, or
re-enter secrets after restore.

## Agent identity & permissions

Every connecting agent gets an identity:

1. the `X-Patchbay-Client` header, if present (recommended; stable across agent
   updates and lets you tell two profiles apart), **else**
2. the self-reported `clientInfo.name` from the MCP `initialize`.

Patchbay **does not cryptographically authenticate** agents. Any local process
that can reach `127.0.0.1:39100` can claim any identity. Local access to the
machine is assumed trusted at the OS level; the identity layer is for
*managing* agents, not for defending against a hostile local process.

On top of identity:

- **Approval gate** (off by default): the first time a *never-seen* identity
  connects, a Win32 Allow/Deny dialog blocks `initialize` until you decide.
  Allow remembers it; Deny adds it to the Forbidden list. The gate **fails
  closed** — a client that sends neither `X-Patchbay-Client` nor
  `clientInfo.name` is rejected outright (such a client can never match the
  Forbidden list, so it cannot be allowed to slip through with default-global
  access).
- **Forbidden list**: deny an identity entirely; every request is rejected.
- **Custom permissions**: give an agent its own on/off list independent of the
  global checkboxes.

## Logging & secret redaction

- **Level-1 (diagnostic, always on):** operational/diagnostic messages, rotated,
  no request bodies. Crash lines via the panic hook.
- **Level-2 (request/event, opt-in, off by default):** records each MCP request
  (resolved identity, method, redacted headers, truncated body) and admin/lifecycle
  events (jack add/remove/toggle, permission changes, reload, port rebind, app
  start/stop).
  - Header redaction: `authorization` and anything containing `token`/`key`/
    `cookie`/`secret`/`auth`/`password` → value replaced with `<redacted>`
    (header *names* are kept).
  - Body redaction: sensitive JSON keys inside a logged request's params are
    recursively redacted (e.g. a jack's `headers`/`env` passed to
    `patchbay__add_jack`).
  - Non-UTF-8 values → `<binary>`; long bodies truncated to 300 chars.

Treat all logs as potentially sensitive and redact before sharing regardless.

## Child processes

Upstream stdio jacks are spawned as children of Patchbay (running with your user
privileges) and attached to a Windows **Job Object**. Quitting Patchbay or
unpatching a jack cannot leave orphaned processes behind — the OS reaps them.
Patchbay does **not** sandbox upstream MCP servers; choose jacks the way you'd
choose any other tool that runs code on your machine.
