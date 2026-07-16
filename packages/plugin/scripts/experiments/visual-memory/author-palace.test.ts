import { describe, expect, test } from "bun:test";

import { renderPalace, validate, type SourceMemory, type SpecEntry } from "./author-palace.ts";

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
    test("rejects a 200-character cue", () => {
        expect(() => validate(source(50), specs("x".repeat(200)))).toThrow("cue over budget");
    });

    test("uses the source importance to select the cue limit", () => {
        expect(() => validate(source(70), specs("x".repeat(90), 1))).not.toThrow();
        expect(() => validate(source(69), specs("x".repeat(51), 100))).toThrow(
            "max 50",
        );
    });

    test("returns review-render budget warnings for trial metrics", () => {
        const previous = process.env.PALACE_RENDER_DESPITE_VALIDATOR;
        process.env.PALACE_RENDER_DESPITE_VALIDATOR = "1";
        try {
            expect(validate(source(50), specs("x".repeat(200)))).toEqual([
                "cue over budget for 1: 200 chars (max 50)",
            ]);
        } finally {
            if (previous === undefined) delete process.env.PALACE_RENDER_DESPITE_VALIDATOR;
            else process.env.PALACE_RENDER_DESPITE_VALIDATOR = previous;
        }
    });
});

describe("palace page packing", () => {
    test("stacks fitting continuation bands on the current page", () => {
        const wordCounts = [1_000, 800, 700, 600, 500, 400, 300, 300, 300];
        const rendered = renderPalace(
            wordCounts.map((count, index) => ({
                id: index + 1,
                category: "PROJECT_RULES" as const,
                room: `Room ${index + 1}`,
                cue: "filler ".repeat(count),
                importance: 50,
            })),
        );
        const firstPageBanners = rendered.layoutItems.filter(
            (item) => item.kind === "category" && item.page === 1,
        );

        expect(rendered.pages).toHaveLength(2);
        expect(firstPageBanners).toHaveLength(2);
    });

    test("does not mark a new category as a continuation after a page break", () => {
        const rendered = renderPalace([
            {
                id: 1,
                category: "PROJECT_RULES",
                room: "Rules",
                cue: "filler ".repeat(900),
                importance: 50,
            },
            {
                id: 2,
                category: "ARCHITECTURE",
                room: "Architecture",
                cue: "filler ".repeat(300),
                importance: 50,
            },
        ]);
        const architectureBanner = rendered.palace
            .split("\n")
            .find((line) => line.includes("<ARCHITECTURE"));

        expect(architectureBanner).toBeDefined();
        expect(architectureBanner).not.toContain("CONT.");
    });

    test("uses bounded layout search for many pidgin-glyph rooms", () => {
        const rendered = renderPalace(
            Array.from({ length: 13 }, (_, index) => ({
                id: index + 1,
                category: "PROJECT_RULES" as const,
                room: `Room ${index + 1}`,
                cue: "`cmd` → ⊘ ∵",
                importance: 50,
            })),
        );

        expect(rendered.rooms).toHaveLength(13);
        expect(rendered.palace).toContain("→");
        expect(rendered.pages.length).toBeGreaterThan(0);
    });
});
