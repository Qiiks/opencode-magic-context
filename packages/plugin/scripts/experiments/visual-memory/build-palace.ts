import { strict as assert } from "node:assert";
import { mkdirSync, readdirSync, readFileSync, unlinkSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { renderTextToImages } from "/Users/ufukaltinok/Work/OSS/pxpipe/src/core/library.ts";

const HERE = dirname(fileURLToPath(import.meta.url));
const SOURCE_PATH = "/tmp/visual-memory/trimmed-memories-source.txt";
const OUTPUT_DIR = "/tmp/visual-memory";
const PROSE_IMAGE_BASELINE_TOKENS = 3_431;
const MAX_LINE_CHARS = 152;
const MAX_PALACE_CHARS = 20_000;

type Coverage = {
    sourceMemoryCount: number;
    entryCount: number;
    mergeCount: number;
    representedMemoryCount: number;
    palaceChars: number;
    maxLineChars: number;
    rooms: Array<{
        category: string;
        name: string;
        entryCount: number;
        mergeCount: number;
        memoryCount: number;
        peakImportance: number;
        border: "single" | "double";
        startLine: number;
        endLine: number;
    }>;
    memories: Record<
        string,
        { category: string; room: string; palaceLine: number; mergedInto?: number }
    >;
};

function sourceIds(source: string): number[] {
    return [...source.matchAll(/^#(\d+):/gm)].map((match) => Number(match[1]));
}

function ratio(numerator: number, denominator: number): number {
    return Number((numerator / denominator).toFixed(2));
}

const palace = readFileSync(join(HERE, "palace.txt"), "utf8");
const coverage = JSON.parse(readFileSync(join(HERE, "coverage.json"), "utf8")) as Coverage;
const source = readFileSync(SOURCE_PATH, "utf8");
const ids = sourceIds(source);
const coveredIds = Object.keys(coverage.memories).map(Number);

assert.equal(ids.length, 334, "trimmed source must contain 334 memories");
assert.equal(new Set(ids).size, ids.length, "source memory ids must be unique");
assert.deepEqual(
    [...coveredIds].sort((a, b) => a - b),
    [...ids].sort((a, b) => a - b),
    "coverage sidecar must map every source memory id exactly once",
);
assert.equal(coverage.sourceMemoryCount, ids.length);
assert.equal(coverage.entryCount + coverage.mergeCount, ids.length);
assert.equal(coverage.representedMemoryCount, ids.length);
assert.equal(coverage.palaceChars, palace.length);
assert.ok(palace.length <= MAX_PALACE_CHARS, `palace exceeds ${MAX_PALACE_CHARS} chars`);
assert.ok(!/#\d+/.test(palace), "palace surface must not render memory ids");
const maxLineChars = Math.max(
    ...palace
        .trimEnd()
        .split("\n")
        .map((line) => [...line].length),
);
assert.equal(coverage.maxLineChars, maxLineChars);
assert.ok(maxLineChars <= MAX_LINE_CHARS, `palace line exceeds ${MAX_LINE_CHARS} chars`);
for (const room of coverage.rooms) {
    assert.equal(room.border, room.peakImportance >= 70 ? "double" : "single");
    assert.ok(room.startLine <= room.endLine, `invalid line range for ${room.name}`);
}

mkdirSync(OUTPUT_DIR, { recursive: true });
for (const file of readdirSync(OUTPUT_DIR)) {
    if (/^palace-page\d+\.png$/.test(file)) unlinkSync(join(OUTPUT_DIR, file));
}
const rendered = await renderTextToImages(palace, {
    cols: MAX_LINE_CHARS,
    shrink: true,
    multiCol: 1,
    reflow: false,
    maxCharsPerImage: 28_000,
    maxHeightPx: 4_096,
});
assert.equal(rendered.droppedChars, 0, "glyph atlas dropped palace characters");
assert.equal(
    rendered.pages.length,
    1,
    "the palace must remain one PNG so no room can split across pages",
);

const pageMetrics = [];
for (const [index, page] of rendered.pages.entries()) {
    const path = join(OUTPUT_DIR, `palace-page${index + 1}.png`);
    await Bun.write(path, page.png);
    pageMetrics.push({
        page: index + 1,
        path,
        width: page.width,
        height: page.height,
        imageTokens: Math.ceil((page.width * page.height) / 750),
    });
}
const imageTokens = pageMetrics.reduce((sum, page) => sum + page.imageTokens, 0);
const proseTextTokens = source.length / 4;
const palaceTextTokens = palace.length / 4;
const report = {
    chars: palace.length,
    pages: rendered.pages.length,
    droppedChars: rendered.droppedChars,
    imageTokens,
    textTokenEquivalent: Number(palaceTextTokens.toFixed(2)),
    proseTextTokens: Number(proseTextTokens.toFixed(2)),
    compressionRatios: {
        versusProseAsText: ratio(proseTextTokens, imageTokens),
        versusProseAsImage: ratio(PROSE_IMAGE_BASELINE_TOKENS, imageTokens),
    },
    coverage: {
        sourceMemories: coverage.sourceMemoryCount,
        entries: coverage.entryCount,
        merges: coverage.mergeCount,
        representedMemories: coverage.representedMemoryCount,
    },
    pagesDetail: pageMetrics,
    rooms: coverage.rooms.map((room) => ({
        category: room.category,
        name: room.name,
        entries: room.entryCount,
        merges: room.mergeCount,
        memories: room.memoryCount,
    })),
};
console.log(`chars=${report.chars}`);
console.log(`pages=${report.pages}`);
console.log(`droppedChars=${report.droppedChars}`);
console.log(`imageTokens=${report.imageTokens}`);
console.log(`textTokenEquivalent=${report.textTokenEquivalent}`);
console.log(`versusProseAsText=${report.compressionRatios.versusProseAsText}x`);
console.log(`versusProseAsImage=${report.compressionRatios.versusProseAsImage}x`);
console.log(`RESULT_JSON=${JSON.stringify(report)}`);
