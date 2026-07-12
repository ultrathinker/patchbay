# Security Policy

## Reporting a vulnerability

If you discover a security vulnerability in Patchbay, **please report it
privately** rather than opening a public issue.

- **Email:** universeissilent42@gmail.com
- Please include a description of the issue, steps to reproduce, and the impact
  you observed.
- You should receive an acknowledgement within a few days. Please do not
  disclose the issue publicly until a fix has been released.

## Supported versions

Patchbay is pre-1.x and moving quickly. Only the **latest release** receives
security fixes.

| Version | Supported |
|---------|-----------|
| latest  | ✅        |
| < latest| ❌        |

## Trust boundary

Patchbay is a **local-only** tool by design. Understanding where the trust
boundaries are helps you report and assess issues accurately:

- **Bound to `127.0.0.1`.** The MCP gateway listens strictly on loopback and is
  not reachable from the network. Any issue that assumes remote reachability is
  out of the threat model.
- **Secrets at rest.** API keys, bearer tokens and sensitive environment
  variables in `patchbay.json` are encrypted with Windows **DPAPI**, tied to the
  running Windows user account, at save time. Plaintext secrets are never
  persisted to disk by Patchbay.
- **Upstream MCP servers ("jacks").** Patchbay spawns and proxies to upstream
  MCP servers *you* configure. Those servers run with your user privileges and
  Patchbay does not sandbox them. Choose your jacks the way you'd choose any
  other tool that runs code on your machine.
- **Connected agents.** Patchbay identifies each connecting agent and supports a
  one-time approval gate and per-agent permission lists, but it does not
  authenticate agents cryptographically — any local process that can reach
  `127.0.0.1:39100` can present itself as an agent. Physical/local access to the
  machine is assumed.
- **Request log.** The opt-in Level-2 request log redacts known-sensitive header
  values and sensitive JSON keys, and truncates request bodies. Treat the logs
  as potentially sensitive anyway and redact before sharing.

## Disclosure timeline (guideline)

1. Private report received and acknowledged.
2. Fix developed and validated.
3. Patch release tagged and published.
4. Public advisory (if applicable) after the fix is available.

The project has no formal SLA, but reports are taken seriously and handled as
promptly as the maintainer's availability allows.
