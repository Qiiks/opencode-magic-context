import { afterEach, describe, expect, it } from "bun:test";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
    createPromptSurfaceGuidanceEpochCache,
    createPromptSurfaceRuntime,
} from "./prompt-surface-runtime";

const tempDirs: string[] = [];

function tempDir(): string {
    const directory = mkdtempSync(join(tmpdir(), "prompt-surface-runtime-"));
    tempDirs.push(directory);
    return directory;
}

afterEach(() => {
    for (const directory of tempDirs) {
        rmSync(directory, { recursive: true, force: true });
    }
    tempDirs.length = 0;
});

describe("prompt-surface runtime", () => {
    it("rejects guidance overrides with zero or two section markers", () => {
        for (const [name, content, expectedCount] of [
            ["zero.md", "Custom guidance without a section marker", 0],
            ["two.md", "## Magic Context\n\nFirst\n\n## Magic Context\n\nSecond", 2],
        ] as const) {
            const directory = tempDir();
            writeFileSync(join(directory, name), content);
            const warnings: string[] = [];
            const runtime = createPromptSurfaceRuntime({
                userConfigDirectory: directory,
                warn: (warning) => warnings.push(warning),
            });

            const selection = runtime.resolveGuidance(
                { default: "full", guidance_override_path: name },
                "provider/model",
            );

            expect(selection.primaryOverride).toBeUndefined();
            expect(warnings).toHaveLength(1);
            expect(warnings[0]).toContain("must contain exactly one");
            expect(warnings[0]).toContain(`found ${expectedCount}`);
        }
    });

    it("captures a valid relative override once per model-key epoch", () => {
        const directory = tempDir();
        const path = join(directory, "guidance.md");
        const first = "## Magic Context\n\nFirst epoch";
        const second = "## Magic Context\n\nSecond epoch";
        writeFileSync(path, first);
        const runtime = createPromptSurfaceRuntime({
            userConfigDirectory: directory,
            warn: () => undefined,
        });
        const epochs = createPromptSurfaceGuidanceEpochCache(runtime);
        const config = {
            default: "full" as const,
            models: { "provider/light": "light" as const },
            guidance_override_path: "guidance.md",
        };

        const initial = epochs.resolve("session", config, "provider/full");
        writeFileSync(path, second);
        const fiveDeferred = Array.from({ length: 5 }, () =>
            epochs.resolve("session", config, "provider/full"),
        );
        const changedModel = epochs.resolve("session", config, "provider/light");

        expect(initial.primaryOverride).toBe(first);
        expect(fiveDeferred.every((selection) => selection === initial)).toBe(true);
        expect(changedModel.preset).toBe("light");
        expect(changedModel.primaryOverride).toBe(second);
    });

    it("uses registration default, applies known overrides, and reports invalid IDs", () => {
        const warnings: string[] = [];
        const runtime = createPromptSurfaceRuntime({
            userConfigDirectory: tempDir(),
            warn: (warning) => warnings.push(warning),
        });
        const registration = runtime.resolveRegistration({
            default: "full",
            models: { "provider/model": "light" },
            tool_descriptions: {
                ctx_search: "Custom search description",
                unknown_tool: "Not allowed",
            },
        });

        expect(registration.preset).toBe("full");
        expect(registration.descriptionFor("ctx_search", "Full search")).toBe(
            "Custom search description",
        );
        expect(registration.descriptionFor("ctx_reduce", "Full reduce")).toBe("Full reduce");
        expect(warnings).toHaveLength(1);
        expect(warnings[0]).toContain("unknown_tool");
    });

    it("logs the temporary light fallback once across registration and guidance", () => {
        const warnings: string[] = [];
        const runtime = createPromptSurfaceRuntime({
            userConfigDirectory: tempDir(),
            warn: (warning) => warnings.push(warning),
        });
        const config = { default: "light" as const };

        const registration = runtime.resolveRegistration(config);
        const guidance = runtime.resolveGuidance(config, "provider/model");
        runtime.resolveGuidance(config, "provider/other");

        expect(registration.descriptionFor("ctx_search", "Full search")).toBe("Full search");
        expect(guidance.preset).toBe("light");
        expect(warnings).toHaveLength(1);
        expect(warnings[0]).toContain("built-in light assets are not available yet");
    });
});
