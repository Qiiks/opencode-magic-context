import { describe, expect, test } from "bun:test";

import { MURAL_HEIGHT, MURAL_WIDTH, type MuralSpecEntry, renderMural } from "./render-mural";

describe("mural renderer", () => {
    test("renders deterministic bytes and balances the five category shares", () => {
        const specs: MuralSpecEntry[] = [
            "PROJECT_RULES",
            "ARCHITECTURE",
            "CONSTRAINTS",
            "CONFIG_VALUES",
            "NAMING",
        ].flatMap((category, categoryIndex) =>
            Array.from({ length: 8 }, (_, index) => ({
                id: categoryIndex * 100 + index,
                category,
                room: `Synthetic room ${categoryIndex}`,
                importance: 80 - index,
                cue: `synthetic cue ${categoryIndex} ${index}`,
            })),
        );
        const first = renderMural(specs);
        const second = renderMural(specs);
        expect(Buffer.from(first.png).equals(Buffer.from(second.png))).toBe(true);
        expect(first.png.slice(0, 8)).toEqual(new Uint8Array([137, 80, 78, 71, 13, 10, 26, 10]));
        expect(first.png.length).toBeGreaterThan(1000);
        expect(first.muralText.split("\n").length).toBe(122);
        expect(Object.values(first.categoryLineUsage).every((value) => value > 0)).toBe(true);
        expect(first.renderedIds.length).toBeGreaterThan(0);
        expect(first.layoutItems.filter((item) => item.kind === "category").length).toBe(5);
    });

    test("never places a room header on the last line of a column", () => {
        const specs: MuralSpecEntry[] = Array.from({ length: 180 }, (_, index) => ({
            id: index + 1,
            category: "PROJECT_RULES",
            room: `Room ${index}`,
            importance: 50,
            cue: "long synthetic cue that fills a line",
        }));
        const result = renderMural(specs);
        expect(
            result.layoutItems
                .filter((item) => item.kind === "room")
                .every((item) => item.startLine <= 121),
        ).toBe(true);
        expect(result.png.length).toBeGreaterThan(0);
        expect([MURAL_WIDTH, MURAL_HEIGHT]).toEqual([1092, 1092]);
    });
});
