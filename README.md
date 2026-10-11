> [!IMPORTANT]
> Remove this line to confirm you've reviewed this PR before submitting.

# Zed CLI

A fork of [Zed](https://github.com/zed-industries/zed) that runs the Claude Code and Codex command line clients inside Zed, with their sessions listed in the threads sidebar.

## 1. Why this fork

Many people prefer the official command line clients of their agents over the agent panel. But Zed's threads sidebar only knows about threads from the agent panel, so CLI sessions are invisible to it.

This fork adds a setting, `agent.use_cli`, which is `true` by default. When it is `true`:

- Starting a thread from the sidebar runs `claude` or `codex` in the terminal panel.
- Clicking a thread resumes its session, or focuses its terminal tab if it is still running.
- Titles stay in sync both ways. Renaming a thread renames the CLI session.
- The agent panel button is hidden, and the terminal panel no longer opens an empty shell.
- The left dock takes the free width while no file is open.

Nothing changes when the setting is `false`. The rest of Zed is untouched.

To turn it off and go back to the agent panel, see [Turn it off](#turn-it-off).

The code is on the [`use-cli-agents-upstream`](https://github.com/nickykln/zed/tree/use-cli-agents-upstream) branch. Known limitations are listed in the [first pull request](https://github.com/nickykln/zed/pull/1).

## 2. Installation

1. Download `Zed-aarch64.dmg` from the [latest release](https://github.com/nickykln/zed/releases/latest).
2. Open the DMG and drag the app into `/Applications`.
3. The app is not notarized. On first launch, right-click > Open, or run:

   ```sh
   xattr -cr "/Applications/Zed Dev.app"
   ```

4. That's it: CLI agents are on by default.

Requirements:

- macOS on Apple Silicon (no Intel build).
- The `claude` and/or `codex` command installed and available in your `PATH`.

The app is named "Zed Dev" and can be installed next to the official Zed.

## Turn it off

Add this to your Zed settings (`cmd-,`, then open the settings file) to go back to the agent panel:

```json
{ "agent": { "use_cli": false } }
```

The change applies right away. Remove the line to turn CLI agents back on.

## Upstream

For everything else (features, documentation, contributing), see [zed-industries/zed](https://github.com/zed-industries/zed). Zed is licensed under the terms of its original repository.
