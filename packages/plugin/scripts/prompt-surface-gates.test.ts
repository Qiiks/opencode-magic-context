import { describe, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { validateChecklist } from "./check-prompt-surface";
import { validateBudgetFixture } from "./prompt-surface-fixture";
import { renderChecklist } from "./render-prompt-surface-checklist";

const rootDir = resolve(import.meta.dir, "../../..");
const fixturePath = resolve(rootDir, "docs/specs/prompt-surface/budget-fixture.json");
const checklistPath = resolve(rootDir, "docs/specs/prompt-surface/checklist.json");
const renderedChecklistPath = resolve(rootDir, "docs/specs/prompt-surface/load-bearing-rules-checklist.md");

function withTempDir<T>(run: (directory: string) => T): T {
    const directory = mkdtempSync(join("/tmp", "prompt-surface-gate-"));
    try {
        return run(directory);
    } finally {
        rmSync(directory, { recursive: true, force: true });
    }
}

describe("prompt-surface CI gates", () => {
    test("fixture baseline drift is a red gate", () => {
        withTempDir((directory) => {
            const fixture = JSON.parse(readFileSync(fixturePath, "utf8")) as {
                mutableProseBaseline: number;
            };
            fixture.mutableProseBaseline += 1;
            const mutatedPath = join(directory, "budget-fixture.json");
            writeFileSync(mutatedPath, JSON.stringify(fixture));

            const result = validateBudgetFixture({ fixturePath: mutatedPath });
            expect(result.errors.some((error) => error.includes("mutable-prose baseline drifted"))).toBe(true);
        });
    });

    test("a light candidate above the ceiling is a red gate", () => {
        withTempDir((directory) => {
            const lightPath = join(directory, "light-surface.json");
            const longText = "x".repeat(10_000);
            writeFileSync(
                lightPath,
                JSON.stringify({
                    variant: "primary-full-reduce-memory-on",
                    guidance: longText,
                    descriptions: {
                        ctx_reduce: longText,
                        ctx_expand: longText,
                        ctx_note: longText,
                        ctx_memory: longText,
                        ctx_search: longText,
                    },
                }),
            );

            const result = validateBudgetFixture({ fixturePath, lightSurfacePath: lightPath });
            expect(result.errors.some((error) => error.includes("exceeds ceiling"))).toBe(true);
        });
    });

    test("rendered checklist matches the machine-readable artifact", () => {
        const checklist = JSON.parse(readFileSync(checklistPath, "utf8"));
        expect(renderChecklist(checklist)).toBe(readFileSync(renderedChecklistPath, "utf8"));
    });

    test("deleting a checklist entry is a red completeness gate", () => {
        withTempDir((directory) => {
            const checklist = JSON.parse(readFileSync(checklistPath, "utf8")) as {
                rules: Array<{ id: string }>;
            };
            checklist.rules.pop();
            const mutatedPath = join(directory, "checklist.json");
            writeFileSync(mutatedPath, JSON.stringify(checklist));

            const result = validateChecklist(mutatedPath);
            expect(result.errors.some((error) => error.includes("checklist entries missing"))).toBe(true);
        });
    });
});
