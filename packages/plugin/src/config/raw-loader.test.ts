import { afterEach, describe, expect, it } from "bun:test";
import { chmodSync, mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { parseJsonc } from "../shared/jsonc-parser";
import { hasFlatKeys, loadRawConfigFile, migrateFlatDetailed } from "./raw-loader";

const temporaryDirectories: string[] = [];

function temporaryDirectory(): string {
    const directory = mkdtempSync(join(tmpdir(), "mc-per-harness-"));
    temporaryDirectories.push(directory);
    return directory;
}

function parse(text: string): Record<string, unknown> {
    const withoutBom = text.startsWith("\uFEFF") ? text.slice(1) : text;
    return parseJsonc<Record<string, unknown>>(withoutBom);
}

afterEach(() => {
    for (const directory of temporaryDirectories.splice(0)) {
        rmSync(directory, { recursive: true, force: true });
    }
});

describe("per-harness raw config migration", () => {
    it("maps flat agent and task fields without moving scheduling fields", () => {
        const input = `\uFEFF{\r
  // preserve this comment\r
  "historian": {\r
    "model": { "model": "provider/historian", "variant": "fast", "thinking_level": "high" }, // historian model note\r
    "fallback_models": [{ "model": "provider/fallback", "variant": "careful", "thinking_level": "low" }],\r
    "variant": "block-variant",\r
    "thinking_level": "medium"\r
  },\r
  "dreamer": {\r
    "model": "provider/dreamer",\r
    "fallback_models": "provider/dreamer-fallback",\r
    "tasks": {\r
      "review": {\r
        "schedule": "0 3 * * *",\r
        "model": { "model": "provider/task", "variant": "task-variant", "thinking_level": "max" },\r
        "thinking_level": "high",\r
        "timeout_minutes": 45\r
      },\r
      "timeout-only": { "schedule": "", "timeout_minutes": 15 }\r
    }\r
  },\r
  "unrelated": { "keep": true }\r
}\r
`;

        const migrated = migrateFlatDetailed(Buffer.from(input, "utf-8"));
        const repeated = migrateFlatDetailed(Buffer.from(input, "utf-8"));
        const output = migrated.bytes.toString("utf-8");
        const config = parse(output);
        const historian = config.historian as Record<string, unknown>;
        const dreamer = config.dreamer as Record<string, unknown>;
        const opencodeHistorian = historian.opencode as Record<string, unknown>;
        const piHistorian = historian.pi as Record<string, unknown>;
        const opencodeDreamer = dreamer.opencode as Record<string, unknown>;
        const piDreamer = dreamer.pi as Record<string, unknown>;
        const tasks = dreamer.tasks as Record<string, Record<string, unknown>>;

        expect(migrated.hasFlatKeys).toBe(true);
        expect(repeated.bytes).toEqual(migrated.bytes);
        expect(repeated.diagnostics).toEqual(migrated.diagnostics);
        expect(hasFlatKeys(migrated.bytes)).toBe(false);
        expect(output.startsWith("\uFEFF")).toBe(true);
        expect(output).toContain("// preserve this comment");
        expect(output).toContain("// historian model note");
        expect(output).toContain("\r\n");
        expect(config.unrelated).toEqual({ keep: true });
        expect(historian.model).toBeUndefined();
        expect(opencodeHistorian).toEqual({
            model: { model: "provider/historian", variant: "fast" },
            fallback_models: [{ model: "provider/fallback", variant: "careful" }],
            variant: "block-variant",
        });
        expect(piHistorian).toEqual({
            model: { model: "provider/historian", thinking_level: "high" },
            fallback_models: [{ model: "provider/fallback", thinking_level: "low" }],
            thinking_level: "medium",
        });
        expect(opencodeDreamer.fallback_models).toEqual(["provider/dreamer-fallback"]);
        expect(piDreamer.fallback_models).toEqual(["provider/dreamer-fallback"]);
        expect(tasks.review).toEqual({ schedule: "0 3 * * *" });
        expect(tasks["timeout-only"]).toEqual({ schedule: "" });
        expect((opencodeDreamer.tasks as Record<string, Record<string, unknown>>).review).toEqual({
            model: { model: "provider/task", variant: "task-variant" },
            timeout_minutes: 45,
        });
        expect((piDreamer.tasks as Record<string, Record<string, unknown>>).review).toEqual({
            model: { model: "provider/task", thinking_level: "max" },
            thinking_level: "high",
            timeout_minutes: 45,
        });
        expect(
            (opencodeDreamer.tasks as Record<string, Record<string, unknown>>)["timeout-only"],
        ).toEqual({ timeout_minutes: 15 });
        expect(
            (piDreamer.tasks as Record<string, Record<string, unknown>>)["timeout-only"],
        ).toEqual({ timeout_minutes: 15 });
    });

    it("keeps new-shape destinations on conflicts and removes redundant flat fields", () => {
        const input = `{
  "historian": {
    "model": "provider/flat",
    "fallback_models": "provider/fallback",
    "opencode": { "model": "provider/opencode", "fallback_models": ["provider/fallback"] },
    "pi": { "model": "provider/pi", "fallback_models": ["provider/fallback"] }
  }
}`;

        const migrated = migrateFlatDetailed(Buffer.from(input));
        const config = parse(migrated.bytes.toString("utf-8"));
        const historian = config.historian as Record<string, unknown>;

        expect(historian.model).toBeUndefined();
        expect(historian.fallback_models).toBeUndefined();
        expect(historian.opencode).toEqual({
            model: "provider/opencode",
            fallback_models: ["provider/fallback"],
        });
        expect(historian.pi).toEqual({
            model: "provider/pi",
            fallback_models: ["provider/fallback"],
        });
        expect(migrated.diagnostics.map((diagnostic) => diagnostic.path)).toEqual([
            "historian.model",
            "historian.model",
        ]);
    });

    it("rewrites user config atomically with one exact-byte restricted backup", () => {
        const directory = temporaryDirectory();
        const configPath = join(directory, "magic-context.jsonc");
        const original = Buffer.from(
            '{\n  "dreamer": { "tasks": { "review": { "timeout_minutes": 30 } } }\n}\n',
        );
        writeFileSync(configPath, original);
        chmodSync(configPath, 0o600);

        const first = loadRawConfigFile({ configPath, tier: "user" });
        const second = loadRawConfigFile({ configPath, tier: "user" });
        const backupPath = `${configPath}.pre-per-harness.bak`;

        expect(first?.migrated).toBe(true);
        expect(first?.warnings).toContain(
            "Migrated flat historian/dreamer model config to per-harness blocks.",
        );
        expect(readFileSync(backupPath)).toEqual(original);
        expect(readFileSync(configPath)).toEqual(first?.bytes);
        expect(statSync(configPath).mode & 0o777).toBe(0o600);
        expect(statSync(backupPath).mode & 0o777).toBe(0o600);
        expect(second?.migrated).toBe(false);
        expect(second?.warnings).toEqual([]);
    });

    it("reloads the winning candidate when a concurrent loader replaces the target", () => {
        const directory = temporaryDirectory();
        const configPath = join(directory, "magic-context.jsonc");
        const original = Buffer.from('{\n  "historian": { "model": "provider/model" }\n}\n');
        writeFileSync(configPath, original);

        let competing: ReturnType<typeof loadRawConfigFile> | undefined;
        const first = loadRawConfigFile({
            configPath,
            tier: "user",
            afterTemporaryWrite: () => {
                competing ??= loadRawConfigFile({ configPath, tier: "user" });
            },
        });

        expect(competing?.migrated).toBe(true);
        expect(first?.migrated).toBe(false);
        expect(first?.warnings).toEqual([]);
        expect(first?.bytes).toEqual(competing?.bytes);
        expect(readFileSync(configPath)).toEqual(competing?.bytes);
        expect(readFileSync(`${configPath}.pre-per-harness.bak`)).toEqual(original);
    });

    it("adapts project config in memory without changing bytes or mtime", () => {
        const directory = temporaryDirectory();
        const configPath = join(directory, "magic-context.jsonc");
        const original = Buffer.from(
            '{\n  "dreamer": { "tasks": { "review": { "timeout_minutes": 30 } } }\n}\n',
        );
        writeFileSync(configPath, original);
        const before = statSync(configPath);

        const loaded = loadRawConfigFile({ configPath, tier: "project" });
        const after = statSync(configPath);
        const config = parse(loaded?.text ?? "{}");

        expect(loaded?.migrated).toBe(false);
        expect(loaded?.warnings[0]).toContain("Adapted flat model config in memory");
        expect(readFileSync(configPath)).toEqual(original);
        expect(after.mtimeMs).toBe(before.mtimeMs);
        expect(
            ((config.dreamer as Record<string, unknown>).opencode as Record<string, unknown>).tasks,
        ).toEqual({ review: { timeout_minutes: 30 } });
    });
});
