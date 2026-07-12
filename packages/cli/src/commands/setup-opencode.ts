import { existsSync, mkdirSync } from "node:fs";
import { basename, dirname } from "node:path";
import { detectConflicts } from "@magic-context/core/shared/conflict-detector";
import { fixConflicts } from "@magic-context/core/shared/conflict-fixer";
import { stringify as stringifyJsonc } from "comment-json";
import { isDevPathPluginEntry, matchesPluginEntry } from "../adapters/opencode";
import { writeFileAtomic } from "../lib/atomic-write";
import {
    hasUserConfigLocationMigrationRefusal,
    migrateConfigLocationsForCli,
} from "../lib/config-location-migration";
import { runDreamerSetup } from "../lib/dreamer-setup";
import { assertJsoncConfigsParseable, readJsoncConfigForUpdate } from "../lib/jsonc-config";
import { pickModel } from "../lib/model-picker";
import { detectOpenCode } from "../lib/opencode-detect";
import { getAvailableModels, getOpenCodeVersion } from "../lib/opencode-helpers";
import {
    OPENCODE_PLUGIN_ENTRY_WITH_VERSION as PLUGIN_ENTRY,
    OPENCODE_PLUGIN_NAME as PLUGIN_NAME,
} from "../lib/opencode-plugin-cache";
import { detectConfigPaths } from "../lib/paths";
import { confirm, intro, log, note, outro, promptIO, spinner } from "../lib/prompts";

const DCP_PLUGIN_NAME = "@tarquinen/opencode-dcp";

// ─── Helpers ──────────────────────────────────────────────

function ensureDir(dir: string): void {
    if (!existsSync(dir)) {
        mkdirSync(dir, { recursive: true });
    }
}

// ─── Config Manipulators ──────────────────────────────────

export function addPluginToOpenCodeConfig(
    configPath: string,
    format: "json" | "jsonc" | "none",
    removeDcp = false,
): void {
    if (format === "none") {
        ensureDir(dirname(configPath));
        const config = {
            plugin: [PLUGIN_ENTRY],
            compaction: { auto: false, prune: false },
        };
        writeFileAtomic(configPath, `${stringifyJsonc(config, null, 2)}\n`);
        return;
    }

    // Read existing config, merge our changes, preserve everything else
    const existing = readJsoncConfigForUpdate(configPath);

    // Operate on the raw plugin array — entries can be:
    //   • a string:  "@cortexkit/opencode-magic-context@latest"
    //   • a tuple:   ["@pkg/name@latest", { ...options }]
    //   • a dev URL: "file:///abs/path/.../packages/plugin"
    // We preserve every entry shape; matchesPluginEntry / isDevPathPluginEntry
    // safely accept both strings and tuples.
    let rawPlugins: unknown[] = Array.isArray(existing.plugin) ? existing.plugin : [];
    if (removeDcp) {
        rawPlugins = rawPlugins.filter((plugin) => !matchesPluginEntry(plugin, DCP_PLUGIN_NAME));
    }
    const hasNpmEntry = rawPlugins.some((p) => matchesPluginEntry(p, PLUGIN_NAME));
    const hasDevEntry = rawPlugins.some((p) => isDevPathPluginEntry(p));

    // Don't double-add if either an npm entry OR a local dev-path entry exists.
    // Dev paths are intentionally NOT replaced — that would silently disable
    // the developer's local plugin checkout.
    if (!hasNpmEntry && !hasDevEntry) {
        rawPlugins.push(PLUGIN_ENTRY);
    }
    existing.plugin = rawPlugins;

    // Set compaction fields without replacing other compaction settings
    const compaction = (existing.compaction as Record<string, unknown>) ?? {};
    compaction.auto = false;
    compaction.prune = false;
    existing.compaction = compaction;

    writeFileAtomic(configPath, `${stringifyJsonc(existing, null, 2)}\n`);
}

export function addPluginToTuiConfig(configPath: string, format: "json" | "jsonc" | "none"): void {
    if (format === "none") {
        ensureDir(dirname(configPath));
        writeFileAtomic(configPath, `${stringifyJsonc({ plugin: [PLUGIN_ENTRY] }, null, 2)}\n`);
        return;
    }

    const existing = readJsoncConfigForUpdate(configPath);

    // Same rules as the main opencode config — preserve tuple entries and
    // never replace dev-path entries.
    const rawPlugins: unknown[] = Array.isArray(existing.plugin) ? existing.plugin : [];
    const hasNpmEntry = rawPlugins.some((p) => matchesPluginEntry(p, PLUGIN_NAME));
    const hasDevEntry = rawPlugins.some((p) => isDevPathPluginEntry(p));

    if (!hasNpmEntry && !hasDevEntry) {
        rawPlugins.push(PLUGIN_ENTRY);
    }

    existing.plugin = rawPlugins;
    writeFileAtomic(configPath, `${stringifyJsonc(existing, null, 2)}\n`);
}

export function findDcpPluginIndexes(plugins: unknown[]): number[] {
    return plugins
        .map((plugin, index) => (matchesPluginEntry(plugin, DCP_PLUGIN_NAME) ? index : -1))
        .filter((index) => index >= 0);
}

function pluginEntryName(entry: unknown): string {
    if (typeof entry === "string") return entry;
    if (Array.isArray(entry) && typeof entry[0] === "string") return entry[0];
    return String(entry);
}

async function resolveDcpConflictBeforeSetup(
    configPath: string,
    format: "json" | "jsonc" | "none",
): Promise<boolean> {
    if (format === "none") return false;
    const ocConfig = readJsoncConfigForUpdate(configPath);
    const plugins = Array.isArray(ocConfig.plugin) ? ocConfig.plugin : [];
    const dcpIndexes = findDcpPluginIndexes(plugins);
    if (dcpIndexes.length === 0) return false;

    log.warn(`Found conflicting plugin: ${pluginEntryName(plugins[dcpIndexes[0]])}`);
    log.message(
        "opencode-dcp (Dynamic Context Pruning) and Magic Context both manage context.\n" +
            "Running both simultaneously will cause unpredictable behavior.",
    );
    const shouldRemove = await confirm("Remove opencode-dcp from your config?", true);
    if (!shouldRemove) {
        log.warn("Skipped — you may experience context management conflicts");
    }
    return shouldRemove;
}

export function writeMagicContextConfig(
    configPath: string,
    options: {
        historianModel: string | null;
        dreamerEnabled: boolean;
        dreamerModel: string | null;
        /** Per-task schedule overrides (Dreamer v2); undefined keeps schema defaults. */
        dreamerTasks?: Record<string, { schedule: string }>;
        sidekickEnabled: boolean;
        sidekickModel: string | null;
        claudeMax: boolean;
    },
): void {
    // A malformed existing file must abort rather than become an empty config.
    const config = readJsoncConfigForUpdate(configPath);

    // Always set $schema for editor autocomplete/validation
    if (!config.$schema) {
        config.$schema =
            "https://raw.githubusercontent.com/cortexkit/magic-context/master/assets/magic-context.schema.json";
    }

    if (options.historianModel) {
        const historian = (config.historian as Record<string, unknown>) ?? {};
        historian.model = options.historianModel;
        config.historian = historian;
    }

    const dreamer = (config.dreamer as Record<string, unknown>) ?? {};
    delete dreamer.enabled;
    if (options.dreamerEnabled) {
        delete dreamer.disable;
        if (options.dreamerModel) {
            dreamer.model = options.dreamerModel;
        }
        // Dreamer v2 per-task schedules. Only written when the user declined the
        // recommended defaults — otherwise we leave `tasks` unset so the schema
        // defaults apply (and the config stays small).
        if (options.dreamerTasks) {
            dreamer.tasks = options.dreamerTasks;
        }
    } else {
        dreamer.disable = true;
    }
    config.dreamer = dreamer;

    const sidekick = (config.sidekick as Record<string, unknown>) ?? {};
    delete sidekick.enabled;
    if (options.sidekickEnabled) {
        delete sidekick.disable;
        if (options.sidekickModel) {
            sidekick.model = options.sidekickModel;
        }
        config.sidekick = sidekick;
    } else {
        sidekick.disable = true;
        config.sidekick = sidekick;
    }

    if (options.claudeMax) {
        const cacheTtl = (config.cache_ttl as Record<string, string>) ?? {};
        if (!cacheTtl.default) cacheTtl.default = "5m";
        cacheTtl["anthropic/claude-sonnet-4-6"] = "59m";
        cacheTtl["anthropic/claude-opus-4-6"] = "59m";
        config.cache_ttl = cacheTtl;
    }

    writeFileAtomic(configPath, `${stringifyJsonc(config, null, 2)}\n`);
}
// ─── Main Setup Flow ──────────────────────────────────────

export async function runSetup(dryRun = false): Promise<number> {
    intro("Magic Context — Setup");
    if (dryRun) {
        log.warn("Dry run — no files will be written and no config will be changed.");
        log.message(
            "[dry-run] would migrate legacy Magic Context config before setup reads or writes the shared CortexKit config.",
        );
    } else {
        const migrationWarnings = migrateConfigLocationsForCli(process.cwd(), log);
        if (hasUserConfigLocationMigrationRefusal(migrationWarnings)) {
            outro(
                "Setup stopped — resolve the legacy Magic Context user config migration conflict, then rerun setup.",
            );
            return 1;
        }
    }

    // ─── Step 1: Check OpenCode ─────────────────────────
    const s = spinner();
    s.start("Checking OpenCode installation");

    const detection = detectOpenCode();
    if (detection.kind === "none") {
        s.stop("OpenCode not found");
        const shouldContinue = await confirm(
            "OpenCode not found on PATH. Continue setup anyway?",
            false,
        );
        if (!shouldContinue) {
            log.info("Install OpenCode: https://opencode.ai");
            outro("Setup cancelled");
            return 1;
        }
    } else if (detection.kind === "desktop") {
        // OpenCode Desktop ships no invocable `opencode` CLI on any OS (its
        // server runs as a JS sidecar inside Electron), so `opencode models`
        // and `opencode --version` are unavailable. Recognize the install and
        // fall through to manual model entry instead of claiming OpenCode is
        // absent.
        s.stop("OpenCode Desktop detected (CLI not installed)");
        log.info(
            "Model auto-discovery needs the OpenCode CLI; you will enter models manually. Install the CLI to auto-populate: https://opencode.ai",
        );
    } else {
        const version = getOpenCodeVersion(detection.binary);
        s.stop(`OpenCode ${version ?? ""} detected`);
    }

    // ─── Step 2: Get available models ───────────────────
    s.start("Fetching available models");

    // Only the CLI can enumerate the authed/resolved model list; Desktop-only
    // installs have no on-disk equivalent, so models stay empty and the model
    // prompts fall back to free-text entry. Use the resolved binary path so a
    // stock CLI that is not on PATH still enumerates.
    const allModels = detection.kind === "cli" ? getAvailableModels(detection.binary) : [];
    if (allModels.length > 0) {
        s.stop(`Found ${allModels.length} models`);
    } else {
        s.stop("No models found");
        log.warn("You can configure models manually in magic-context.jsonc later");
    }

    // ─── Step 3: Detect config paths ────────────────────
    const paths = detectConfigPaths();
    const hadExistingSetup =
        paths.opencodeConfigFormat !== "none" ||
        existsSync(paths.magicContextConfig) ||
        paths.tuiConfigFormat !== "none";

    if (!dryRun) {
        try {
            // Fail before touching any setup target if one existing file is malformed.
            assertJsoncConfigsParseable([
                paths.opencodeConfig,
                paths.magicContextConfig,
                paths.tuiConfig,
            ]);
        } catch (error) {
            log.error(error instanceof Error ? error.message : String(error));
            outro("Setup stopped — fix the malformed config and rerun setup.");
            return 1;
        }
    }

    // ─── Step 4: Check for DCP plugin conflict before mutating setup files ────────
    const removeDcp = dryRun
        ? false
        : await resolveDcpConflictBeforeSetup(paths.opencodeConfig, paths.opencodeConfigFormat);

    // Collect every interactive choice before applying setup writes. A cancelled
    // wizard can then unwind without leaving only some target files updated.
    if (dryRun) {
        log.message(
            `[dry-run] would add the plugin to ${paths.opencodeConfig} and disable compaction`,
        );
    }

    let conflictFix: Parameters<typeof fixConflicts>[1] | null = null;
    if (hadExistingSetup) {
        const conflicts = detectConflicts(process.cwd());
        if (conflicts.hasConflict) {
            log.warn("Found conflicting configuration that can disable Magic Context:");
            for (const reason of conflicts.reasons) {
                log.message(`  • ${reason}`);
            }

            if (dryRun) {
                log.message("[dry-run] would offer to apply automatic conflict fixes");
            } else {
                const shouldFixConflicts = await confirm(
                    "Apply automatic conflict fixes to your OpenCode and OMO config files?",
                    true,
                );

                if (shouldFixConflicts) {
                    conflictFix = conflicts.conflicts;
                } else {
                    log.warn(
                        "Skipped automatic conflict fixes — Magic Context may remain disabled",
                    );
                }
            }
        }
    }

    // ─── Step 5: Historian model ────────────────────────
    // pickModel shows the full discovered list when non-empty; when discovery
    // returns [] it still runs and offers free-text provider/model entry (same as Pi setup).
    const historianModel = await pickModel(promptIO, allModels, "historian");
    log.success(`Historian: ${historianModel}`);

    // ─── Step 6: Dreamer ────────────────────────────────
    const dreamerEnabled = await confirm("Enable dreamer?", true);
    let dreamerModel: string | null = null;
    let dreamerTasks: Record<string, { schedule: string }> | undefined;
    if (dreamerEnabled) {
        const result = await runDreamerSetup(promptIO, allModels);
        dreamerModel = result.model;
        dreamerTasks = result.tasks;
    }

    // ─── Step 7: Sidekick ───────────────────────────────
    const sidekickEnabled = await confirm("Enable sidekick?", false);
    let sidekickModel: string | null = null;
    if (sidekickEnabled) {
        sidekickModel = await pickModel(promptIO, allModels, "sidekick");
        log.success(`Sidekick: ${sidekickModel}`);
    }

    // ─── Claude Max subscription ────────────────────────
    const hasAnthropic = allModels.some((m) => m.startsWith("anthropic/"));
    let claudeMax = false;
    if (hasAnthropic) {
        log.message(
            "Claude Max/Pro subscribers get extended prompt caching (up to 1 hour).\n" +
                "This lets Magic Context defer context operations much longer, saving money.",
        );
        claudeMax = await confirm("Do you have a Claude Max or Pro subscription?", false);
        if (claudeMax) {
            log.success("Cache TTL set to 59m for Anthropic models");
        }
    }

    if (dryRun) {
        log.message(`[dry-run] would write Magic Context config to ${paths.magicContextConfig}`);
        log.message(`[dry-run] would add the TUI sidebar plugin to ${paths.tuiConfig}`);
    }

    // ─── Step 8: Oh-My-OpenCode compatibility ───────────
    // Intentional: this branch handles the FIRST-TIME-INSTALL case only.
    // Existing users hit the same OMO conflict-fix logic via the
    // `if (hadExistingSetup) detectConflicts/fixConflicts` block above
    // (lines 231-257), which already covers omoPreemptiveCompaction,
    // omoContextWindowMonitor, and omoAnthropicRecovery. Audit tools
    // sometimes flag this `!hadExistingSetup` gate as "OMO check skipped
    // for existing users" — that's a false positive.
    let disableOmoHooks = false;
    if (paths.omoConfig && !hadExistingSetup) {
        log.warn(`Found oh-my-opencode config: ${paths.omoConfig}`);
        log.message(
            "These hooks may conflict:\n" +
                "  • context-window-monitor\n" +
                "  • preemptive-compaction\n" +
                "  • anthropic-context-window-limit-recovery",
        );

        const shouldDisable = dryRun
            ? false
            : await confirm("Disable these hooks in oh-my-opencode?", true);
        if (dryRun) {
            log.message("[dry-run] would offer to disable conflicting oh-my-opencode hooks");
        }
        if (shouldDisable) {
            disableOmoHooks = true;
        } else {
            log.warn("Skipped — you may experience context management conflicts");
        }
    }

    if (!dryRun) {
        addPluginToOpenCodeConfig(paths.opencodeConfig, paths.opencodeConfigFormat, removeDcp);
        log.success(`Plugin added to ${paths.opencodeConfig}`);
        if (removeDcp) log.success("Removed opencode-dcp from plugin list");
        log.info("Disabled built-in compaction (auto=false, prune=false)");
        log.message(
            "Magic Context handles context management — built-in compaction would interfere",
        );

        if (conflictFix) {
            const actions = fixConflicts(process.cwd(), conflictFix);
            if (actions.length > 0) {
                for (const action of actions) log.success(action);
            } else {
                log.info("No additional conflict changes were needed");
            }
        }

        writeMagicContextConfig(paths.magicContextConfig, {
            historianModel,
            dreamerEnabled,
            dreamerModel,
            dreamerTasks,
            sidekickEnabled,
            sidekickModel,
            claudeMax,
        });
        log.success(`Config written to ${paths.magicContextConfig}`);
        addPluginToTuiConfig(paths.tuiConfig, paths.tuiConfigFormat);
        log.success(`TUI sidebar plugin added to ${basename(paths.tuiConfig)}`);

        if (disableOmoHooks) {
            const actions = fixConflicts(process.cwd(), {
                compactionAuto: false,
                compactionPrune: false,
                dcpPlugin: false,
                omoPreemptiveCompaction: true,
                omoContextWindowMonitor: true,
                omoAnthropicRecovery: true,
            });
            if (actions.includes("Disabled conflicting oh-my-opencode hooks")) {
                log.success("Hooks disabled in oh-my-opencode config");
            }
        }
    }

    // ─── Summary ────────────────────────────────────────
    const summary = [
        `Plugin: ${PLUGIN_NAME}`,
        "Compaction: disabled",
        historianModel ? `Historian: ${historianModel}` : "Historian: fallback chain",
        dreamerEnabled
            ? `Dreamer: enabled${dreamerModel ? ` (${dreamerModel})` : ""}`
            : "Dreamer: disabled",
        sidekickEnabled
            ? `Sidekick: enabled${sidekickModel ? ` (${sidekickModel})` : ""}`
            : "Sidekick: disabled",
    ].join("\n");

    note(summary, dryRun ? "Configuration (dry run — not written)" : "Configuration");

    if (dryRun) {
        outro("Dry run complete — nothing was written.");
        return 0;
    }

    // Ask user to star the repo
    const shouldStar = await confirm("★ Star the repo on GitHub?", true);
    if (shouldStar) {
        try {
            const { execSync } = await import("node:child_process");
            execSync("gh api --silent --method PUT /user/starred/cortexkit/magic-context", {
                stdio: "ignore",
                timeout: 10_000,
            });
            log.success("Thanks for starring! ★");
        } catch {
            log.info(
                "Couldn't star automatically. You can star manually:\n  https://github.com/cortexkit/magic-context",
            );
        }
    }

    outro("Run 'opencode' to start!");

    return 0;
}
