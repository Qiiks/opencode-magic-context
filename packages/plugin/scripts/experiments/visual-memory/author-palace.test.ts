import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { describe, expect, test } from "bun:test";

import {
    authorPalace,
    renderPalace,
    validate,
    type SourceMemory,
    type SpecEntry,
} from "./author-palace.ts";

function source(importance: number): SourceMemory[] {
    return [{ id: 1, category: "PROJECT_RULES", importance }];
}

function specs(cue: string, importance = 50): SpecEntry[] {
    return [
        {
            id: 1,
            category: "PROJECT_RULES",
            room: "Hub",
            cue,
            importance,
        },
    ];
}

describe("palace cue budgets", () => {
    test("reports a 200-character cue as a warning", () => {
        expect(validate(source(50), specs("x".repeat(200)))).toEqual([
            "cue over budget for 1: 200 chars (max 50)",
        ]);
    });

    test("uses the source importance to select the cue warning limit", () => {
        expect(validate(source(70), specs("x".repeat(90), 1))).toEqual([]);
        expect(validate(source(69), specs("x".repeat(51), 100))).toEqual([
            "cue over budget for 1: 51 chars (max 50)",
        ]);
    });

    test("allows selected manifests to omit source memories", () => {
        expect(
            validate(
                [
                    { id: 1, category: "PROJECT_RULES", importance: 50 },
                    { id: 2, category: "PROJECT_RULES", importance: 40 },
                ],
                specs("selected"),
            ),
        ).toEqual([]);
    });

    test("keeps duplicate ids hard", () => {
        expect(() =>
            validate(source(50), [
                ...specs("first"),
                { ...specs("second")[0]!, cue: "second" },
            ]),
        ).toThrow("duplicate spec id");
    });
});

describe("single-page newspaper flow", () => {
    test("fills one fixed page with a continuous three-column stream", () => {
        const rendered = renderPalace(
            [...Array(10)].flatMap((_, roomIndex) =>
                [...Array(150)].map((__, entryIndex) => ({
                    id: roomIndex * 150 + entryIndex + 1,
                    category: "PROJECT_RULES" as const,
                    room: `Room ${roomIndex + 1}`,
                    cue: `value-${roomIndex * 150 + entryIndex + 1}`,
                    importance: 100 - roomIndex,
                })),
            ),
        );
        const lines = rendered.palace.trimEnd().split("\n");
        const width = rendered.layoutItems[0] ? 72 : 0;
        const pitch = width + 1;
        expect(rendered.pages).toHaveLength(1);
        expect(rendered.layoutItems.filter((item) => item.kind === "category")).toHaveLength(1);
        expect([0, 1, 2].map((column) =>
            lines.filter((line) => line.slice(column * pitch, column * pitch + width).trim()).length,
        )).toEqual([121, 121, 121]);
    });

    test("never leaves a room header as the last line of a column", () => {
        const rendered = renderPalace(
            [...Array(13)].map((_, index) => ({
                id: index + 1,
                category: "PROJECT_RULES" as const,
                room: `Room ${index + 1}`,
                cue: "`cmd` → ⊘ ∵",
                importance: 50,
            })),
        );
        for (const item of rendered.layoutItems) {
            if (item.kind === "room" && !item.continuation) {
                expect(item.startLine).toBeLessThan(121);
            }
        }
        expect(rendered.palace).not.toMatch(/[╔╗╚╝┌┐└┘│═]{2,}/);
    });

    test("splits a room across columns without dropping its entries", () => {
        const entries = [...Array(300)].map((_, index) => ({
            id: index + 1,
            category: "PROJECT_RULES" as const,
            room: "Long room",
            cue: `value-${index + 1}`,
            importance: 50,
        }));
        const rendered = renderPalace(entries);
        expect(rendered.placements.size).toBe(entries.length);
        expect(rendered.layoutItems.filter((item) => item.kind === "room" && item.continuation).length).toBeGreaterThan(0);
        expect(rendered.palace).toContain("— Long room —");
    });

    test("globally interleaves a high-importance later-category room", () => {
        const rendered = renderPalace([
            { id: 1, category: "PROJECT_RULES", room: "Early low", cue: "policy", importance: 10 },
            { id: 2, category: "ARCHITECTURE", room: "Later high", cue: "policy", importance: 90 },
        ]);
        expect(rendered.layoutItems.find((item) => item.kind === "room")?.category).toBe("ARCHITECTURE");
    });

    test("keeps trim and skip sidecar coverage distinct", () => {
        const directory = mkdtempSync(join(tmpdir(), "palace-knapsack-"));
        try {
            const sourceMemories = Array.from({ length: 6 }, (_, index) => ({
                id: index + 1,
                category: "PROJECT_RULES" as const,
                importance: index < 3 ? 90 : 10,
            }));
            const rendered = authorPalace({
                source: sourceMemories,
                specs: [
                    ...[1, 2, 3].map((id) => ({
                        id,
                        category: "PROJECT_RULES" as const,
                        room: "High",
                        cue: "filler ".repeat(1_300),
                        importance: 90,
                    })),
                    ...[4, 5].map((id) => ({
                        id,
                        category: "PROJECT_RULES" as const,
                        room: "Low",
                        cue: "filler ".repeat(2_000),
                        importance: 10,
                    })),
                    { id: 6, category: "PROJECT_RULES" as const, room: "Tiny", cue: "small", importance: 1 },
                ],
                palaceOutput: join(directory, "palace.txt"),
                coverageOutput: join(directory, "coverage.json"),
            });
            const coverage = JSON.parse(readFileSync(join(directory, "coverage.json"), "utf8")) as {
                renderedIds: number[];
                droppedByTrimIds: number[];
                droppedBySkipIds: number[];
                renderedMemoryCount: number;
                droppedMemoryCount: number;
                memories: Record<string, unknown>;
            };
            expect(rendered.coverage.layout.pages).toHaveLength(1);
            expect(coverage.renderedIds).toEqual([1, 2, 6]);
            expect(coverage.droppedByTrimIds).toEqual([3]);
            expect(coverage.droppedBySkipIds).toEqual([4, 5]);
            expect(coverage.renderedMemoryCount).toBe(3);
            expect(coverage.droppedMemoryCount).toBe(3);
            expect(Object.keys(coverage.memories).map(Number).sort((a, b) => a - b)).toEqual([1, 2, 6]);
        } finally {
            rmSync(directory, { recursive: true, force: true });
        }
    });
});
