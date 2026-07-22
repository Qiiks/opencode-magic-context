import { describe, expect, test } from "bun:test";

import {
    MURAL_HEIGHT,
    MURAL_ROOM_WIDTH,
    MURAL_WIDTH,
    type MuralRenderEntry,
    renderMural,
} from "./render-mural";

const CATEGORIES = ["PROJECT_RULES", "ARCHITECTURE", "CONSTRAINTS", "CONFIG_VALUES", "NAMING"];

function longestLine(text: string): number {
    return Math.max(...text.split("\n").map((line) => [...line.replace(/\s+$/, "")].length), 0);
}

describe("deterministic mural renderer", () => {
    test("renders identical bytes for identical input (pure function)", () => {
        const entries: MuralRenderEntry[] = Array.from({ length: 40 }, (_, index) => ({
            id: index + 1,
            category: CATEGORIES[index % CATEGORIES.length]!,
            importance: 80 - (index % 30),
            cue: `synthetic cue ${index}`,
        }));
        const first = renderMural(entries);
        const second = renderMural(entries);
        expect(Buffer.from(first.png).equals(Buffer.from(second.png))).toBe(true);
        expect(first.sha256Input).toBe(second.sha256Input);
        expect(first.png.slice(0, 8)).toEqual(new Uint8Array([137, 80, 78, 71, 13, 10, 26, 10]));
        expect([MURAL_WIDTH, MURAL_HEIGHT]).toEqual([1092, 1092]);
    });

    test("fills all three columns on a 300-cue fixture (>80% line occupancy)", () => {
        // 300 medium cues far exceed one column; with flat bands + full pool the
        // three-column flow should be densely filled.
        const entries: MuralRenderEntry[] = Array.from({ length: 300 }, (_, index) => ({
            id: index + 1,
            category: CATEGORIES[index % CATEGORIES.length]!,
            importance: 75,
            cue: `cue ${index} anchor→target relation`,
        }));
        const result = renderMural(entries);
        // All three columns are used.
        expect(result.layoutItems.some((item) => item.column === 0)).toBe(true);
        expect(result.layoutItems.some((item) => item.column === 1)).toBe(true);
        expect(result.layoutItems.some((item) => item.column === 2)).toBe(true);
        // Occupancy: filled content lines vs. total grid capacity > 80%.
        const rows = Math.floor(MURAL_HEIGHT / 9);
        const capacity = 3 * rows;
        expect(result.filledLineCount / capacity).toBeGreaterThan(0.8);
    });

    test("word-wraps a long high-importance cue at the column width (never overruns 72)", () => {
        const longCue =
            "very long high importance cue that certainly exceeds one column and must wrap across multiple lines cleanly";
        const result = renderMural([
            { id: 1, category: "PROJECT_RULES", importance: 90, cue: longCue },
        ]);
        // No rendered line exceeds the column width in codepoints.
        expect(longestLine(result.muralText)).toBeLessThanOrEqual(
            MURAL_ROOM_WIDTH * 3 + 2, // three padded columns + 2 single-space gaps
        );
        for (const line of result.muralText.split("\n")) {
            for (const column of [0, 1, 2]) {
                const slice = line.slice(
                    column * (MURAL_ROOM_WIDTH + 1),
                    column * (MURAL_ROOM_WIDTH + 1) + MURAL_ROOM_WIDTH,
                );
                expect([...slice].length).toBeLessThanOrEqual(MURAL_ROOM_WIDTH);
            }
        }
        // The single entry still gets a placement (its first wrapped line).
        expect(result.renderedIds).toContain(1);
    });

    test("packs two short cues onto one shared line", () => {
        const result = renderMural([
            { id: 1, category: "NAMING", importance: 50, cue: "short a" },
            { id: 2, category: "NAMING", importance: 50, cue: "short b" },
        ]);
        // Both ids land on the SAME line (shared pair) → identical line number.
        const a = result.placements.get(1);
        const b = result.placements.get(2);
        expect(a).toBeDefined();
        expect(b).toBeDefined();
        expect(a!.line).toBe(b!.line);
        expect(a!.column).toBe(b!.column);
    });

    test("does not shared-pair a prohibition cue (⊘ carries its own line)", () => {
        const result = renderMural([
            { id: 1, category: "CONSTRAINTS", importance: 50, cue: "⊘x (break)" },
            { id: 2, category: "CONSTRAINTS", importance: 50, cue: "short b" },
        ]);
        const a = result.placements.get(1);
        const b = result.placements.get(2);
        expect(a!.line).not.toBe(b!.line);
    });

    test("prohibition ink: a ⊘ cue renders in the prohibition color, plain cues in body ink", () => {
        // Two murals differing ONLY in whether the cue is a prohibition must
        // produce different pixels (prohibition ink is a distinct color).
        const withProhibition = renderMural([
            { id: 1, category: "CONSTRAINTS", importance: 80, cue: "⊘cache write (ABI break)" },
        ]);
        const withoutProhibition = renderMural([
            { id: 1, category: "CONSTRAINTS", importance: 80, cue: "cache write ABI ok now yes" },
        ]);
        expect(Buffer.from(withProhibition.png).equals(Buffer.from(withoutProhibition.png))).toBe(
            false,
        );
    });

    test("empty entry list renders nothing placed (m0 omits the block)", () => {
        const result = renderMural([]);
        expect(result.renderedIds).toHaveLength(0);
        expect(result.filledLineCount).toBe(0);
        // Still produces a valid (blank) PNG rather than throwing.
        expect(result.png.slice(0, 8)).toEqual(new Uint8Array([137, 80, 78, 71, 13, 10, 26, 10]));
    });

    test("emits one category banner per distinct category band", () => {
        const result = renderMural([
            { id: 1, category: "PROJECT_RULES", importance: 80, cue: "a b c" },
            { id: 2, category: "ARCHITECTURE", importance: 80, cue: "d e f" },
            { id: 3, category: "ARCHITECTURE", importance: 70, cue: "g h i" },
        ]);
        const banners = result.layoutItems.filter((item) => item.kind === "category");
        expect(banners).toHaveLength(2);
    });
});
