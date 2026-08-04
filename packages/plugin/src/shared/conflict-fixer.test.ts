/// <reference types="bun-types" />

import { afterEach, beforeEach, describe, expect, it } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { parse as parseJsonc } from "comment-json";
import { fixConflicts } from "./conflict-fixer";
import { detectConflicts } from "./conflict-detector";

const noOmoConflicts = {
    omoPreemptiveCompaction: false,
    omoContextWindowMonitor: false,
    omoAnthropicRecovery: false,
};

describe("fixConflicts", () => {
    let root: string;
    let projectDir: string;
    let userConfigDir: string;
    let homeDir: string;
    let originalEnv: Record<string, string | undefined>;

    beforeEach(() => {
        root = mkdtempSync(join(tmpdir(), "mc-conflict-fixer-"));
        projectDir = join(root, "project");
        userConfigDir = join(root, "user-config", "opencode");
        homeDir = join(root, "home");
        mkdirSync(projectDir, { recursive: true });
        mkdirSync(userConfigDir, { recursive: true });
        mkdirSync(homeDir, { recursive: true });
        originalEnv = {
            OPENCODE_CONFIG_DIR: process.env.OPENCODE_CONFIG_DIR,
            XDG_CONFIG_HOME: process.env.XDG_CONFIG_HOME,
            HOME: process.env.HOME,
        };
        process.env.OPENCODE_CONFIG_DIR = userConfigDir;
        process.env.HOME = homeDir;
        delete process.env.XDG_CONFIG_HOME;
    });

    afterEach(() => {
        for (const [key, value] of Object.entries(originalEnv)) {
            if (value === undefined) delete process.env[key];
            else process.env[key] = value;
        }
        try {
            rmSync(root, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
        } catch {
            /* Ignore EBUSY on Windows */
        }
    });

    it("preserves JSONC comments and tuple plugin entries while removing canonical DCP", () => {
        const configPath = join(projectDir, "opencode.jsonc");
        writeFileSync(
            configPath,
            `{
  // keep this file-level comment
  "plugin": [
    ["@plannotator/opencode@latest", { "workflow": "plan-agent" }],
    ["@tarquinen/opencode-dcp@latest", { "enabled": true }],
    "@cortexkit/opencode-magic-context@latest"
  ],
  "compaction": {
    // keep this compaction comment
    "auto": true,
    "prune": true
  }
}
`,
        );

        const actions = fixConflicts(projectDir, {
            compactionAuto: true,
            compactionPrune: true,
            dcpPlugin: true,
            ...noOmoConflicts,
        });

        const updatedText = readFileSync(configPath, "utf-8");
        const updated = parseJsonc(updatedText) as Record<string, unknown>;
        expect(actions).toEqual(["Disabled auto-compaction", "Removed opencode-dcp plugin"]);
        expect(updatedText).toContain("keep this file-level comment");
        expect(updatedText).toContain("keep this compaction comment");
        expect(updated.compaction).toEqual({ auto: false, prune: false });
        expect(updated.plugin).toEqual([
            ["@plannotator/opencode@latest", { workflow: "plan-agent" }],
            "@cortexkit/opencode-magic-context@latest",
        ]);
    });

    it("skips non-existent target files instead of creating user config", () => {
        const actions = fixConflicts(projectDir, {
            compactionAuto: true,
            compactionPrune: true,
            dcpPlugin: true,
            ...noOmoConflicts,
        });

        expect(actions).toEqual([]);
        expect(existsSync(join(userConfigDir, "opencode.json"))).toBe(false);
        expect(existsSync(join(userConfigDir, "opencode.jsonc"))).toBe(false);
    });

    it("keeps DCP forks and substring-only names because matching is canonical", () => {
        const configPath = join(projectDir, "opencode.json");
        writeFileSync(
            configPath,
            JSON.stringify({
                plugin: [
                    "@some-fork/opencode-dcp-fork",
                    "file:///tmp/opencode-dcp-dev",
                    ["@other/opencode-dcp-slim@latest", { enabled: true }],
                ],
            }),
        );

        const actions = fixConflicts(projectDir, {
            compactionAuto: false,
            compactionPrune: false,
            dcpPlugin: true,
            ...noOmoConflicts,
        });

        const updated = parseJsonc(readFileSync(configPath, "utf-8")) as Record<string, unknown>;
        expect(actions).toEqual([]);
        expect(updated.plugin).toEqual([
            "@some-fork/opencode-dcp-fork",
            "file:///tmp/opencode-dcp-dev",
            ["@other/opencode-dcp-slim@latest", { enabled: true }],
        ]);
    });

    // --- Unified OMO config (oh-my-openagent >= 4.19.0) ---

    describe("unified OMO config paths", () => {
        const omoConflicts = {
            compactionAuto: false,
            compactionPrune: false,
            dcpPlugin: false,
            omoPreemptiveCompaction: true,
            omoContextWindowMonitor: true,
            omoAnthropicRecovery: true,
        };

        it("disables hooks inside [opencode] block in ~/.omo/omo.jsonc, comments survive, detector confirms", () => {
            const omoDir = join(homeDir, ".omo");
            mkdirSync(omoDir, { recursive: true });
            const configPath = join(omoDir, "omo.jsonc");
            writeFileSync(
                configPath,
                `{
  // top-level comment
  "some-omo-setting": true,
  "[opencode]": {
    // opencode block comment
    "other_setting": "value"
  }
}
`,
            );

            const actions = fixConflicts(projectDir, omoConflicts);

            expect(actions).toEqual(["Disabled conflicting oh-my-opencode hooks"]);

            const updatedText = readFileSync(configPath, "utf-8");
            expect(updatedText).toContain("top-level comment");
            expect(updatedText).toContain("opencode block comment");
            expect(updatedText).toContain("some-omo-setting");

            const updated = parseJsonc(updatedText) as Record<string, unknown>;
            const opencodeBlock = updated["[opencode]"] as Record<string, unknown>;
            expect(opencodeBlock.disabled_hooks).toEqual([
                "context-window-monitor",
                "preemptive-compaction",
                "anthropic-context-window-limit-recovery",
            ]);
            expect(opencodeBlock.other_setting).toBe("value");

            // Round-trip: detector should now report no OMO conflicts
            // Need a project-level opencode.json with OMO plugin for detection
            writeFileSync(
                join(projectDir, "opencode.json"),
                JSON.stringify({ plugin: ["oh-my-opencode"] }),
            );
            const result = detectConflicts(projectDir);
            expect(result.conflicts.omoPreemptiveCompaction).toBe(false);
            expect(result.conflicts.omoContextWindowMonitor).toBe(false);
            expect(result.conflicts.omoAnthropicRecovery).toBe(false);
        });

        it("creates [opencode] block when missing in unified omo.jsonc", () => {
            const omoDir = join(homeDir, ".omo");
            mkdirSync(omoDir, { recursive: true });
            const configPath = join(omoDir, "omo.jsonc");
            writeFileSync(
                configPath,
                `{
  // just a top-level setting, no [opencode] block
  "some-omo-setting": true
}
`,
            );

            const actions = fixConflicts(projectDir, omoConflicts);

            expect(actions).toEqual(["Disabled conflicting oh-my-opencode hooks"]);

            const updatedText = readFileSync(configPath, "utf-8");
            expect(updatedText).toContain("top-level setting");

            const updated = parseJsonc(updatedText) as Record<string, unknown>;
            const opencodeBlock = updated["[opencode]"] as Record<string, unknown>;
            expect(opencodeBlock.disabled_hooks).toEqual([
                "context-window-monitor",
                "preemptive-compaction",
                "anthropic-context-window-limit-recovery",
            ]);
        });

        it("writes to project-level .omo/omo.jsonc", () => {
            const omoDir = join(projectDir, ".omo");
            mkdirSync(omoDir, { recursive: true });
            const configPath = join(omoDir, "omo.jsonc");
            writeFileSync(
                configPath,
                JSON.stringify({
                    "[opencode]": {},
                }),
            );

            const actions = fixConflicts(projectDir, omoConflicts);

            expect(actions).toEqual(["Disabled conflicting oh-my-opencode hooks"]);

            const updated = parseJsonc(readFileSync(configPath, "utf-8")) as Record<string, unknown>;
            const opencodeBlock = updated["[opencode]"] as Record<string, unknown>;
            expect(opencodeBlock.disabled_hooks).toEqual([
                "context-window-monitor",
                "preemptive-compaction",
                "anthropic-context-window-limit-recovery",
            ]);
        });

        it("reads omo.json (fallback) when omo.jsonc does not exist", () => {
            const omoDir = join(homeDir, ".omo");
            mkdirSync(omoDir, { recursive: true });
            const configPath = join(omoDir, "omo.json");
            writeFileSync(
                configPath,
                JSON.stringify({
                    "[opencode]": {},
                }),
            );

            const actions = fixConflicts(projectDir, omoConflicts);

            expect(actions).toEqual(["Disabled conflicting oh-my-opencode hooks"]);

            const updated = JSON.parse(readFileSync(configPath, "utf-8"));
            expect(updated["[opencode]"].disabled_hooks).toEqual([
                "context-window-monitor",
                "preemptive-compaction",
                "anthropic-context-window-limit-recovery",
            ]);
        });

        it("updates both legacy and unified config when both exist", () => {
            // Legacy: project-level oh-my-opencode.json
            const legacyPath = join(projectDir, "oh-my-opencode.json");
            writeFileSync(legacyPath, JSON.stringify({ disabled_hooks: [] }));

            // Unified: ~/.omo/omo.jsonc
            const omoDir = join(homeDir, ".omo");
            mkdirSync(omoDir, { recursive: true });
            const unifiedPath = join(omoDir, "omo.jsonc");
            writeFileSync(
                unifiedPath,
                JSON.stringify({
                    "[opencode]": {},
                }),
            );

            const actions = fixConflicts(projectDir, omoConflicts);

            expect(actions).toEqual(["Disabled conflicting oh-my-opencode hooks"]);

            // Legacy: top-level disabled_hooks
            const legacy = JSON.parse(readFileSync(legacyPath, "utf-8"));
            expect(legacy.disabled_hooks).toEqual([
                "context-window-monitor",
                "preemptive-compaction",
                "anthropic-context-window-limit-recovery",
            ]);

            // Unified: inside [opencode] block
            const unified = JSON.parse(readFileSync(unifiedPath, "utf-8"));
            expect(unified["[opencode]"].disabled_hooks).toEqual([
                "context-window-monitor",
                "preemptive-compaction",
                "anthropic-context-window-limit-recovery",
            ]);
        });

        it("skips non-existent unified paths (no create)", () => {
            // No .omo directory at all — fixer should find no targets
            const actions = fixConflicts(projectDir, omoConflicts);
            expect(actions).toEqual([]);
        });
    });
});
