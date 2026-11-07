# Security model

This document expands on the trust boundary in [`SECURITY.md`](../SECURITY.md)
with the implementation details an auditor or contributor needs.

## Principle: local-only by construction

Patchbay binds its MCP gateway **strictly to `127.0.0.1:39100`**. It is not
network-reachable; reaching it requires running code on the local machine as the
same OS user (Windows or macOS). This is the single most important property:
most "remote attacker" scenarios are out of scope by design.

## Secrets at rest — DPAPI (Windows) and the login Keychain (macOS)

Sensitive values in `patchbay.json` are encrypted with the secret store of the
OS Patchbay runs on, bound to the current user account:

- jack `env` map values,
- jack `headers` map values (e.g. `Authorization`).

The platform-neutral facade is `utils/secret_store.rs`; each encrypted value
carries a prefix that says how to open it.

| OS | Mechanism | Stored as |
|---|---|---|
| Windows | **DPAPI**, CurrentUser scope (`CryptProtectData`) | `dpapi:<base64>` |
| macOS | **AES-256-GCM** with a random 256-bit key kept in the user's login Keychain (generic password, service `dev.patchbay.app`, account `config-encryption-key`) | `keychain:<base64(nonce‖ciphertext)>` |

On save, every plaintext sensitive value is encrypted and rewritten with the
platform's prefix; re-saving a value that already carries **either** prefix is a
no-op (no double encryption). At use time the values are decrypted in memory and
passed to the spawned child / HTTP request. **Plaintext secrets are never
persisted to disk** by Patchbay after the first save.

On macOS the Keychain key is created **only if the Keychain item does not
exist**. If Keychain access fails (access denied, Keychain locked), Patchbay
reports the error and does **not** make a new key — a new key would orphan every
secret already sealed under the old one.

Threat model: equivalent to DPAPI. Any process running as the same user could
ask the OS for the DPAPI-protected data or the Keychain key, so this does not
defend against hostile code running as you. What it does give you is that a
copied `patchbay.json` — a backup, a synced folder, another machine — is useless
without your account's DPAPI keys or your Keychain.

Consequence: a `patchbay.json` copied to another user account or machine cannot
be decrypted there. A value encrypted on the *other* OS (a config copied between
a Windows and a macOS machine) cannot be opened either: it decrypts to an empty
value and the log says to re-enter it. Back up the config together with the user
profile (on macOS, with the Keychain), or re-enter secrets after restore.

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
  connects, a native Allow/Deny dialog blocks `initialize` until you decide —
  `MessageBoxW` on Windows, an AppleScript `display dialog` run by
  `/usr/bin/osascript` on macOS. Allow remembers it; Deny adds it to the
  Forbidden list. The gate **fails
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
privileges), and Patchbay makes sure they do not outlive it:

- **Windows** — each child is attached to a **Job Object** with
  `KILL_ON_JOB_CLOSE`. Quitting Patchbay or unpatching a jack cannot leave
  orphaned processes behind — the OS reaps the whole tree (cmd → npx → node),
  even if Patchbay crashes or is killed.
- **macOS** — each child leads its **own process group**. Unpatching a jack kills
  the whole group (e.g. `npx` and the `node` it started); quitting Patchbay —
  tray Quit, window Quit, logout, or SIGTERM / SIGINT / SIGHUP — kills
  every group. **Limitation:** there is no OS-level kill-on-crash on macOS. If
  Patchbay itself is force-killed (SIGKILL) or crashes, nothing reaps the
  children; they see EOF on stdin, which ends a well-behaved MCP server but is
  not guaranteed.

Patchbay does **not** sandbox upstream MCP servers; choose jacks the way you'd
choose any other tool that runs code on your machine. On macOS Patchbay also
imports your login shell's `PATH` at startup (`$SHELL -l -i -c`, 5 s timeout) so
that commands like `npx` resolve; this means your shell profile is executed
once at startup, with your privileges, exactly as opening a terminal would.

## The window UI (S13) — untrusted strings in a webview

Since 1.3.0 Patchbay can optionally show a popover **window** (`ui_mode` =
`window` or `both`; the default `tray` creates no window at all). The window is
a local webview (WebView2 on Windows, the system WKWebView on macOS) that loads
only assets embedded in the binary, under the strict CSP declared in
`tauri.conf.json` — no CDN, no remote origin, no network fetch.

That introduces one threat the native menu did not have, and it is worth stating
plainly because it is easy to get wrong.

**Agent identity is attacker-controlled text.** As described above, the
`X-Patchbay-Client` header and `clientInfo.name` are self-reported and
unauthenticated. In the tray menu those strings are handed to the OS's native
menu (Win32 `AppendMenuW` on Windows), which treats them as text and nothing
else. In a webview the
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
   (On macOS there is a second interpreter for this text: the approval and
   delete-agent dialogs are built as AppleScript, so the identity is escaped
   there as well — see below.)
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
credentials, not even the encrypted forms (`dpapi:` / `keychain:`). Secrets
travel in one direction only: they may be typed into the "Add server" form and
sent once to the backend, which persists them encrypted. They are never sent
back, so an existing jack's secret cannot be read out of the UI.

## Native dialogs — the identity string and AppleScript (macOS)

The approval dialog and the "delete agent" confirmation show the agent's
identity, which is attacker-controlled text (see above). On Windows it is passed
to `MessageBoxW` as a plain wide string. On macOS the dialogs are AppleScript
`display dialog` calls run through `/usr/bin/osascript`, which would make the
identity a string literal inside a script — a quote or backslash in a crafted
name could otherwise break out of the literal and add AppleScript of its own.
So the identity is **escaped for AppleScript** before it is placed in the
script: backslashes and double quotes are escaped, control characters are
dropped, and line breaks become `& return &`, so nothing in the name can end the
string literal. (This sits on top of the source-level sanitizing above; neither
layer is relied on alone.)

Both dialogs run on a plain thread, never on the async runtime or the UI thread,
and fail safe: dismissing the dialog, pressing Escape, or a failure to show it
counts as **Deny** for the approval gate and as **Cancel** for deletion — and the
delete confirmation's default button is Cancel, so a stray Return deletes
nothing.

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

`patchbay.json` holds every server definition and every encrypted secret.
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
