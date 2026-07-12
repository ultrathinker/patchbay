# Contributing to Patchbay

Thanks for your interest in Patchbay! This project is a Windows-only Rust/Tauri
v2 system-tray app, so a few things are specific to that stack.

## Reporting issues

- Open a [GitHub issue](https://github.com/ultrathinker/Patchbay/issues) for
  bugs and feature requests.
- **Security vulnerabilities must be reported privately** — see
  [SECURITY.md](SECURITY.md). Do **not** open a public issue for a security
  problem.
- Include your Windows version, Patchbay version (tray → About), and the
  relevant log lines from `%APPDATA%\Patchbay\logs\` if you enabled request
  logging. Redact any secrets before pasting.

## Development setup

Prerequisites:

- **Rust** (stable toolchain).
- **Visual Studio 2022 BuildTools** with the C++ workload (the `windows` crate
  needs the MSVC toolchain — `vcvars64.bat`). On a machine with multiple VS
  installs, the included `_build.ps1` captures the complete BuildTools
  environment explicitly.

Build and test (run from `src-tauri/`, or use the helpers from the repo root):

```bash
# Debug build + tests
cargo build
cargo test

# Optimized release binary
cargo build --release

# Windows NSIS installer (needs the Tauri prerequisites)
cargo tauri build
```

The helpers wrap the MSVC environment setup:

```powershell
powershell -File _test.ps1     # cargo test (debug)
powershell -File _build.ps1    # cargo build (debug)
powershell -File _build.ps1 -Release
```

## Code style

- Match the style of the surrounding code. The codebase uses
  `parking_lot` non-poisoning locks and a project `log()` helper
  (`utils/log.rs`) — prefer it over `println!`/`eprintln!` for anything that is
  not a bootstrap fallback.
- No new secrets, tokens or personal data in source, configs, tests or logs.
  Secrets live in `patchbay.json` and are DPAPI-encrypted by the app itself.
- Keep commits focused and write clear messages.

## Pull request process

1. Fork the repo and create a branch from `main`.
2. Make sure `cargo test` passes (and `cargo build`, ideally `cargo build
   --release`) on Windows.
3. If you change user-visible behavior, update `ABOUT.txt`, `README.md` and
   `CHANGELOG.md` accordingly.
4. Open a pull request against `main` and fill in the PR template. Reference any
   related issue (`Closes #123`).

## Scope

Patchbay intentionally stays small and Windows-focused. Features that would
require a visible window, a network listener beyond `127.0.0.1`, or a generic
MCP SDK (e.g. `rmcp`) are out of scope by design. If in doubt, open an issue to
discuss before starting work.
