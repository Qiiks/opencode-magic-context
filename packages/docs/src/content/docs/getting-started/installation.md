---
title: Installation
description: How to install Magic Context on OpenCode or Pi using the interactive setup wizard, and how to verify your install.
---

import { Tabs, TabItem } from '@astrojs/starlight/components';

The setup wizard detects which harnesses you have installed, configures the plugin, and handles compatibility conflicts automatically. Run it once to get started.

## Requirements

- **Node.js >= 24** for the CLI
- **OpenCode** (current version) — for OpenCode installs
- **Pi >= 0.74.0** — for Pi installs

## Run setup

```bash
npx @cortexkit/magic-context@latest setup
```

The wizard auto-detects whether you have OpenCode, Pi, or both installed. It then:

1. Registers the plugin in your harness config
2. Disables built-in compaction (OpenCode only — Magic Context replaces it)
3. Prompts you to pick models for the historian and dreamer agents
4. Resolves any conflicts with other context-management plugins
5. Writes a `magic-context.jsonc` config file with sensible defaults

To target one harness explicitly, pass `--harness opencode` or `--harness pi`.

:::note
**Why is compaction disabled?** Magic Context manages context itself. OpenCode's built-in compaction would interfere with the historian and double-compress your history. Setup turns it off automatically. See [compatibility](/help/compatibility/) for details on other plugins that conflict.
:::

## What gets configured

<Tabs>
<TabItem label="OpenCode">

Setup adds the plugin to your `opencode.jsonc` and turns off compaction:

```jsonc
{
  "plugin": ["@cortexkit/opencode-magic-context@latest"],
  "compaction": { "auto": false, "prune": false }
}
```

It also creates a `magic-context.jsonc` config file in the shared CortexKit location (the same for both harnesses; project overrides user):

| Path | Scope |
|---|---|
| `<project>/.cortexkit/magic-context.jsonc` | Project |
| `~/.config/cortexkit/magic-context.jsonc` | User-wide defaults |

</TabItem>
<TabItem label="Pi">

Setup adds the extension to Pi's settings and creates a `magic-context.jsonc` config file in the shared CortexKit location (the same for both harnesses; project overrides user):

| Path | Scope |
|---|---|
| `<project>/.cortexkit/magic-context.jsonc` | Project |
| `~/.config/cortexkit/magic-context.jsonc` | User-wide defaults |

:::note
Pi setup prompts for `thinking_level` if you pick a `github-copilot/*` reasoning model — Copilot requires it and rejects the default value Pi would send otherwise. The wizard handles this for you.
:::

</TabItem>
</Tabs>

## Manual setup (OpenCode)

If you cannot run the wizard, add this to `opencode.jsonc`:

```jsonc
{
  "plugin": ["@cortexkit/opencode-magic-context@latest"],
  "compaction": { "auto": false, "prune": false }
}
```

Then create `magic-context.jsonc` with the one setting the historian needs:

```jsonc
{
  "$schema": "https://raw.githubusercontent.com/cortexkit/magic-context/master/assets/magic-context.schema.json",
  "historian": { "model": "provider/model-id" }
}
```

- **Required:** `historian.model` must be a real `provider/model-id`. Without it, the plugin loads but historian runs fail, older history is not summarized, and repeated failures show a `Magic Context — history comparting needs attention` notice.
- **Optional:** `dreamer` and `sidekick` model/disable blocks. Omit them to leave periodic memory consolidation and `/ctx-aug` off.
- **Optional:** `embedding`. Omit it to use the local `Xenova/all-MiniLM-L6-v2`; turning embeddings off removes semantic/embedding-backed search, but keyword search and context management continue.

User-level config is `~/.config/cortexkit/magic-context.jsonc` on macOS/Linux and `%USERPROFILE%\.config\cortexkit\magic-context.jsonc` on Windows (or `$XDG_CONFIG_HOME/cortexkit/magic-context.jsonc` when set). OpenCode Desktop users can use the dashboard's config editor or hand-edit that file; Desktop does not include the CLI setup wizard.

## Verify the install

After setup, restart your harness (reload OpenCode or restart Pi) so the plugin loads.

<Tabs>
<TabItem label="OpenCode">

Run `/ctx-status` in the OpenCode TUI or Desktop. You should see a status view with context usage, tag counts, and historian state. A live sidebar in the TUI also shows a real-time context breakdown after every message.

</TabItem>
<TabItem label="Pi">

Run `/ctx-status` in Pi. You should see a status line with context usage and Magic Context state. Pi also shows a status line in the footer when the plugin is active.

</TabItem>
</Tabs>

## Check an existing install

If Magic Context is already installed and something isn't working, run the doctor:

```bash
npx @cortexkit/magic-context@latest doctor
```

Doctor auto-detects your harnesses and checks: plugin registration, config validity, conflicts, database integrity, and the embedding endpoint. It prints a `PASS X / WARN Y / FAIL Z` summary. If both OpenCode and Pi are installed, doctor asks which harnesses to diagnose; use `--harness opencode` or `--harness pi` to select one without prompting. When it finds multiple OpenCode installations, it prints a table with the active path, version, and source so a shadowed binary is visible.

Add `--force` to automatically fix what doctor can — it clears stale plugin caches and repairs common config issues. Add `--issue` to generate a sanitized bug report ready to file.

## Model configuration

The setup wizard helps you pick a model for the historian and dreamer agents — they don't need a top-tier model, and a model that bills per request (e.g. GitHub Copilot) keeps background-work cost flat. There's no hidden fallback to models you didn't configure; see the [configuration reference](/reference/configuration/) for `model` and optional `fallback_models`.

## Dashboard

Magic Context ships a companion desktop app for browsing memories, session history, cache diagnostics, and dreamer runs. See the [dashboard reference](/reference/dashboard/) or download it from the [GitHub releases page](https://github.com/cortexkit/magic-context/releases).
