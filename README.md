> [!IMPORTANT]
> Remove this line to confirm you've reviewed this PR before submitting.

## About this fork

This is a fork of [Zed](https://github.com/zed-industries/zed) with one extra feature: an opt-in `agent.use_cli` setting.

**Why?** Many people prefer the official command line clients of their agents (Claude Code, Codex) over the agent panel. But Zed's thread sidebar only knows about threads from the agent panel. With this fork, the sidebar keeps organizing your sessions, and the CLI does the work.

**What it does** when `agent.use_cli` is `true`:

- Starting a thread from the sidebar runs `claude` or `codex` in the terminal panel.
- Clicking a thread resumes its session, or focuses its terminal tab if it is still running.
- Titles stay in sync both ways. Renaming a thread renames the CLI session.
- The agent panel button is hidden, and the terminal panel no longer opens an empty shell.
- The left dock takes the free width while no file is open.

Nothing changes when the setting is off, which is the default. The rest of Zed is untouched.

**Try it**

1. [Download the macOS build](https://github.com/nickykln/zed/releases/latest) (Apple Silicon only, not notarized: right-click > Open on first launch).
2. Add this to your settings:

```json
{ "agent": { "use_cli": true } }
```

The code lives on the [`use-cli-agents-upstream`](https://github.com/nickykln/zed/tree/use-cli-agents-upstream) branch. Known limitations are listed in the [first pull request](https://github.com/nickykln/zed/pull/1).

# Zed

[![Zed](https://img.shields.io/endpoint?url=https://raw.githubusercontent.com/zed-industries/zed/main/assets/badge/v0.json)](https://zed.dev)
[![CI](https://github.com/zed-industries/zed/actions/workflows/run_tests.yml/badge.svg)](https://github.com/zed-industries/zed/actions/workflows/run_tests.yml)

Welcome to Zed, a high-performance, multiplayer code editor from the creators of [Atom](https://github.com/atom/atom) and [Tree-sitter](https://github.com/tree-sitter/tree-sitter).

---

### Installation

On macOS, Linux, and Windows you can [download Zed directly](https://zed.dev/download) or install Zed via your local package manager ([macOS](https://zed.dev/docs/installation#macos)/[Linux](https://zed.dev/docs/linux#installing-via-a-package-manager)/[Windows](https://zed.dev/docs/windows#package-managers)).

Other platforms are not yet available:

- Web ([tracking discussion](https://github.com/zed-industries/zed/discussions/26195))

### Developing Zed

- [Building Zed for macOS](./docs/src/development/macos.md)
- [Building Zed for Linux](./docs/src/development/linux.md)
- [Building Zed for Windows](./docs/src/development/windows.md)

### Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md) for ways you can contribute to Zed.

Also... we're hiring! Check out our [jobs](https://zed.dev/jobs) page for open roles.

### Licensing

Zed source code is licensed primarily under GPL-3.0-or-later, with Apache-2.0 components where marked.

License information for third party dependencies must be correctly provided for CI to pass.

We use [`cargo-about`](https://github.com/EmbarkStudios/cargo-about) to automatically comply with open source licenses. If CI is failing, check the following:

- Is it showing a `no license specified` error for a crate you've created? If so, add `publish = false` under `[package]` in your crate's Cargo.toml.
- Is the error `failed to satisfy license requirements` for a dependency? If so, first determine what license the project has and whether this system is sufficient to comply with this license's requirements. If you're unsure, ask a lawyer. Once you've verified that this system is acceptable add the license's SPDX identifier to the `accepted` array in `script/licenses/zed-licenses.toml`.
- Is `cargo-about` unable to find the license for a dependency? If so, add a clarification field at the end of `script/licenses/zed-licenses.toml`, as specified in the [cargo-about book](https://embarkstudios.github.io/cargo-about/cli/generate/config.html#crate-configuration).

## Sponsorship

Zed is developed by **Zed Industries, Inc.**, a for-profit company.

If you’d like to financially support the project, you can do so via GitHub Sponsors.
Sponsorships go directly to Zed Industries and are used as general company revenue.
There are no perks or entitlements associated with sponsorship.
