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

## The window UI (S13) — untrusted strings in a webview

Since 1.3.0 Patchbay can optionally show a popover **window** (`ui_mode` =
`window` or `both`; the default `tray` creates no window at all). The window is
a local WebView2 that loads only assets embedded in the binary, under the strict
CSP declared in `tauri.conf.json` — no CDN, no remote origin, no network fetch.

That introduces one threat the native menu did not have, and it is worth stating
plainly because it is easy to get wrong.

**Agent identity is attacker-controlled text.** As described above, the
`X-Patchbay-Client` header and `clientInfo.name` are self-reported and
unauthenticated. In the tray menu those strings are handed to Win32
(`AppendMenuW`), which treats them as text and nothing else. In a webview the
same string is markup: an agent that names itself with an HTML fragment carrying
an event handler would be executing script inside the UI. Because the Tauri IPC
bridge is reachable from the page, script in the page can call the commands the
page is allowed to call — and one of those creates a jack, which can spawn a
local process. The honest description of that chain is **display-string → script
execution → command execution**, inside a process that holds the user's MCP
credentials.

Patchbay defends this in four independent layers, so no single mistake is fatal:

1. **Sanitized at the source.** A client name is restricted to a safe display
   character set and length-capped when it is recorded; the raw bytes never
   reach a renderer. This also protects the tray label and the log file.
2. **Escaped by construction.** The frontend sets text as text. `innerHTML`,
   `dangerouslySetInnerHTML` and `insertAdjacentHTML` are banned in `dist/`, and
   a CI check fails the build if one appears.
3. **A minimal reachable surface.** Only the `ui_*` commands are exposed to the
   popover window, via a Tauri capability scoped to that window.
4. **A strict CSP**, unchanged by the window work and with no `unsafe-inline` in
   `script-src`.

The same rule covers every other externally-influenced string the UI renders:
client version, upstream error text, jack status, and config-error text.

**Secrets never reach the frontend.** The snapshot the window renders carries
names, flags, counts and statuses only — no `env` values, no headers, no URL
credentials, not even the DPAPI-encrypted forms. Secrets travel in one direction
only: they may be typed into the "Add server" form and sent once to the backend,
which persists them DPAPI-encrypted. They are never sent back, so an existing
jack's secret cannot be read out of the UI.

## Who may switch a server on (S14)

Patchbay exists so that a **person** decides which MCP servers an agent can
reach. Two paths let a caller decide it for themselves:

- the gateway-owned meta tools `patchbay__toggle_jack` and `patchbay__add_jack`,
  callable by any agent that has finished `initialize`; and
- the typed REST routes under `/admin/jacks*`, which carry **no identity at
  all** — anything able to open a socket to `127.0.0.1` can call them, which on
  a developer machine is a great many programs.

Either path could switch a server back on that the user had switched off —
including a production database server — and Patchbay would start it and hand
over the tools, deciding over the Custom lists, the forbidden gate and the
approval dialog alike.

One rule now covers both, in `gateway/policy.rs`:

> **Nothing arriving over the gateway may make a server reachable that was not
> reachable already.**

- `patchbay__toggle_jack` with `patched: false` — allowed. Switching a server
  off only ever reduces what the caller can reach, and it is how an agent drops
  a server it does not need.
- `patchbay__toggle_jack` with `patched: true` — refused, with a message naming
  the tray icon and the window, so the model asks the user instead of retrying.
- `patchbay__add_jack` — allowed only with `patched: false`, which saves a
  definition the user can review and enable. `patched` **defaults to true**, so
  an omitted field is refused as well: the laziest call must not be the one that
  gets through.
- `POST /admin/jacks/{name}/toggle` and `POST /admin/jacks` — identical rules,
  answered with `403`.

The advertised tool schemas state the rule, so a model reads it before spending
a call on it. Refusals are recorded in the Level-2 event log when it is on.

This is deliberately **not** authentication. Identifying loopback callers is a
much larger question; this closes the door that was actually open without
pretending to answer it. `remove_jack` remains available to agents: it destroys
a definition rather than granting access, so it falls outside this rule — worth
revisiting, but not by widening a rule about granting.

## Where the secrets live, and what else shares the file (S14)

`patchbay.json` holds every server definition and every DPAPI-encrypted secret.
It used to hold the list of agents that had connected as well — which meant
ordinary agent traffic rewrote it: an agent connecting appended to the list, and
a `last_seen` timestamp was refreshed roughly once a minute per active agent.
Every one of those writes was a chance to lose the entire configuration for a
reason that had nothing to do with configuration.

The observed list now lives in a sibling `patchbay.state.json`. It contains
nothing secret and is disposable — delete it and previously-known agents simply
look new. An existing install migrates itself on load: the list is still read
from the old file, and the key disappears on the next save.

Two further guards on the same file:

- **A corrupt config is never overwritten.** When `patchbay.json` fails to
  parse, Patchbay keeps an empty default in memory and surfaces the reason in
  the tray. Every write now passes through one guarded helper that refuses while
  that error stands, and a source-level test stops a new mutator from bypassing
  it. Previously only startup refused to save, while every mutator still wrote —
  so a corrupt config had about a minute to live.
- **One generation is kept.** The previous file is copied to
  `patchbay.json.bak` before each atomic replace.
