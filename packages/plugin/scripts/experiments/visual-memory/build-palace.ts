import { strict as assert } from "node:assert";
import { mkdirSync, readdirSync, readFileSync, unlinkSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { renderTextToImages } from "/Users/ufukaltinok/Work/OSS/pxpipe/src/core/library.ts";
import { encodeGrayPng } from "/Users/ufukaltinok/Work/OSS/pxpipe/src/core/png.ts";
import {
    PAD_X,
    PAD_Y,
    renderCellHeight,
    renderCellWidth,
} from "/Users/ufukaltinok/Work/OSS/pxpipe/src/core/render.ts";

const HERE = dirname(fileURLToPath(import.meta.url));
const SOURCE_PATH = "/tmp/visual-memory/trimmed-memories-source.txt";
const OUTPUT_DIR = "/tmp/visual-memory";
const PROSE_IMAGE_BASELINE_TOKENS = 3_431;
const MAX_PALACE_CHARS = 50_000;
const RENDER_STYLE = { aa: true } as const;
const TITLE_SCALE = 2;

type LayoutItem = {
    kind: "category" | "room";
    category: string;
    room?: string;
    column: number;
    startLine: number;
    endLine: number;
};
type CoverageRoom = {
    category: string;
    name: string;
    entryCount: number;
    mergeCount: number;
    memoryCount: number;
    peakImportance: number;
    border: "single" | "double";
    column: number;
    startLine: number;
    endLine: number;
    heightCells: number;
};
type Coverage = {
    sourceMemoryCount: number;
    entryCount: number;
    mergeCount: number;
    representedMemoryCount: number;
    palaceChars: number;
    maxLineChars: number;
    layout: {
        columns: number;
        roomWidthChars: number;
        columnGapChars: number;
        canvasHeightCells: number;
        items: LayoutItem[];
    };
    rooms: CoverageRoom[];
    memories: Record<
        string,
        {
            category: string;
            room: string;
            palaceLine: number;
            palaceColumn: number;
            mergedInto?: number;
        }
    >;
};
type GrayImage = { pixels: Uint8Array; width: number; height: number };
type Panel = GrayImage & { droppedChars: number; item: LayoutItem };

function sourceIds(source: string): number[] {
    return [...source.matchAll(/^#(\d+):/gm)].map((match) => Number(match[1]));
}

function ratio(numerator: number, denominator: number): number {
    return Number((numerator / denominator).toFixed(2));
}

function concat(parts: Uint8Array[]): Uint8Array {
    const length = parts.reduce((sum, part) => sum + part.length, 0);
    const result = new Uint8Array(length);
    let offset = 0;
    for (const part of parts) {
        result.set(part, offset);
        offset += part.length;
    }
    return result;
}

async function inflateZlib(input: Uint8Array): Promise<Uint8Array> {
    const stream = new DecompressionStream("deflate");
    const writer = stream.writable.getWriter();
    void writer.write(input as Uint8Array<ArrayBuffer>);
    void writer.close();
    const reader = stream.readable.getReader();
    const chunks: Uint8Array[] = [];
    while (true) {
        const { value, done } = await reader.read();
        if (done) break;
        if (value) chunks.push(value);
    }
    return concat(chunks);
}

async function decodeGrayPng(png: Uint8Array): Promise<GrayImage> {
    const signature = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
    assert.deepEqual([...png.subarray(0, 8)], signature, "invalid PNG signature");
    let offset = 8;
    let width = 0;
    let height = 0;
    const idat: Uint8Array[] = [];
    while (offset < png.length) {
        const length = new DataView(png.buffer, png.byteOffset + offset, 4).getUint32(0, false);
        const type = String.fromCharCode(...png.subarray(offset + 4, offset + 8));
        const data = png.subarray(offset + 8, offset + 8 + length);
        if (type === "IHDR") {
            const view = new DataView(data.buffer, data.byteOffset, data.byteLength);
            width = view.getUint32(0, false);
            height = view.getUint32(4, false);
            assert.equal(data[8], 8, "compositor expects 8-bit PNG input");
            assert.equal(data[9], 0, "compositor expects grayscale PNG input");
        } else if (type === "IDAT") {
            idat.push(data);
        } else if (type === "IEND") {
            break;
        }
        offset += length + 12;
    }
    assert.ok(width > 0 && height > 0 && idat.length > 0, "incomplete PNG input");
    const raw = await inflateZlib(concat(idat));
    const stride = width + 1;
    assert.equal(raw.length, stride * height, "unexpected PNG scanline length");
    const pixels = new Uint8Array(width * height);
    for (let y = 0; y < height; y++) {
        assert.equal(raw[y * stride], 0, "compositor expects PNG filter=None");
        pixels.set(raw.subarray(y * stride + 1, (y + 1) * stride), y * width);
    }
    return { pixels, width, height };
}

function crop(
    image: GrayImage,
    left: number,
    top: number,
    right: number,
    bottom: number,
): GrayImage {
    const width = image.width - left - right;
    const height = image.height - top - bottom;
    assert.ok(width > 0 && height > 0, "invalid crop rectangle");
    const pixels = new Uint8Array(width * height);
    for (let y = 0; y < height; y++) {
        const source = (y + top) * image.width + left;
        pixels.set(image.pixels.subarray(source, source + width), y * width);
    }
    return { pixels, width, height };
}

function upscale2x(image: GrayImage): GrayImage {
    const width = image.width * TITLE_SCALE;
    const height = image.height * TITLE_SCALE;
    const pixels = new Uint8Array(width * height);
    for (let y = 0; y < image.height; y++) {
        for (let x = 0; x < image.width; x++) {
            const value = image.pixels[y * image.width + x] ?? 255;
            const target = y * TITLE_SCALE * width + x * TITLE_SCALE;
            pixels[target] = value;
            pixels[target + 1] = value;
            pixels[target + width] = value;
            pixels[target + width + 1] = value;
        }
    }
    return { pixels, width, height };
}

function blitInk(target: GrayImage, source: GrayImage, left: number, top: number): void {
    assert.ok(left >= 0 && top >= 0, "negative blit offset");
    assert.ok(
        left + source.width <= target.width && top + source.height <= target.height,
        "blit exceeds target",
    );
    for (let y = 0; y < source.height; y++) {
        for (let x = 0; x < source.width; x++) {
            const sourceValue = source.pixels[y * source.width + x] ?? 255;
            if (sourceValue === 255) continue;
            const index = (top + y) * target.width + left + x;
            target.pixels[index] = Math.min(target.pixels[index] ?? 255, sourceValue);
        }
    }
}

async function renderPanelText(
    text: string,
    cols: number,
): Promise<GrayImage & { droppedChars: number }> {
    const rendered = await renderTextToImages(text, {
        cols,
        shrink: false,
        multiCol: 1,
        reflow: false,
        maxCharsPerImage: 28_000,
        maxHeightPx: 4_096,
        style: RENDER_STYLE,
    });
    assert.equal(rendered.pages.length, 1, "masonry panel unexpectedly split across pages");
    const page = rendered.pages[0];
    assert.ok(page, "renderer returned no masonry panel");
    const decoded = await decodeGrayPng(page.png);
    return { ...crop(decoded, PAD_X, PAD_Y, PAD_X, PAD_Y), droppedChars: rendered.droppedChars };
}

function extractItemLines(palaceLines: string[], item: LayoutItem, coverage: Coverage): string[] {
    const left = item.column * (coverage.layout.roomWidthChars + coverage.layout.columnGapChars);
    const lines: string[] = [];
    for (let lineNumber = item.startLine; lineNumber <= item.endLine; lineNumber++) {
        const source = [...(palaceLines[lineNumber - 1] ?? "")];
        lines.push(source.slice(left, left + coverage.layout.roomWidthChars).join(""));
    }
    return lines;
}

const palace = readFileSync(join(HERE, "palace.txt"), "utf8");
const coverage = JSON.parse(readFileSync(join(HERE, "coverage.json"), "utf8")) as Coverage;
const source = readFileSync(SOURCE_PATH, "utf8");
const ids = sourceIds(source);
const coveredIds = Object.keys(coverage.memories).map(Number);
const palaceLines = palace.endsWith("\n") ? palace.slice(0, -1).split("\n") : palace.split("\n");

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
assert.equal(palaceLines.length, coverage.layout.canvasHeightCells);
const maxLineChars = Math.max(...palaceLines.map((line) => [...line].length));
assert.equal(coverage.maxLineChars, maxLineChars);
assert.equal(
    maxLineChars,
    coverage.layout.columns * coverage.layout.roomWidthChars +
        (coverage.layout.columns - 1) * coverage.layout.columnGapChars,
    "masonry text canvas width drifted",
);
for (const room of coverage.rooms) {
    assert.equal(room.border, room.peakImportance >= 70 ? "double" : "single");
    assert.equal(room.heightCells, room.endLine - room.startLine + 1);
}

const roomByKey = new Map(
    coverage.rooms.map((room) => [`${room.category}\u0000${room.name}`, room]),
);
const panels: Panel[] = [];
let droppedChars = 0;
for (const item of coverage.layout.items) {
    const lines = extractItemLines(palaceLines, item, coverage);
    if (item.kind === "room") {
        const room = roomByKey.get(`${item.category}\u0000${item.room ?? ""}`);
        assert.ok(room, `coverage room missing for ${item.category}/${item.room}`);
        assert.ok(lines.length >= 5, `room ${room.name} lacks reserved title rows`);
        const side = [...(lines[1] ?? "")][0] ?? "│";
        lines[1] = `${side}${" ".repeat(coverage.layout.roomWidthChars - 2)}${side}`;
        lines[2] = `${side}${" ".repeat(coverage.layout.roomWidthChars - 2)}${side}`;
        const panel = await renderPanelText(lines.join("\n"), coverage.layout.roomWidthChars);
        const title = await renderPanelText(room.name, [...room.name].length);
        const scaledTitle = upscale2x(title);
        const titleLeft = Math.floor((panel.width - scaledTitle.width) / 2);
        const titleTop = renderCellHeight(RENDER_STYLE);
        blitInk(panel, scaledTitle, titleLeft, titleTop);
        droppedChars += panel.droppedChars + title.droppedChars;
        panels.push({ ...panel, droppedChars: panel.droppedChars + title.droppedChars, item });
    } else {
        const panel = await renderPanelText(lines.join("\n"), coverage.layout.roomWidthChars);
        droppedChars += panel.droppedChars;
        panels.push({ ...panel, droppedChars: panel.droppedChars, item });
    }
}

const cellWidth = renderCellWidth(RENDER_STYLE);
const cellHeight = renderCellHeight(RENDER_STYLE);
const columnWidthPixels = coverage.layout.roomWidthChars * cellWidth;
const pageWidth = coverage.layout.columns * columnWidthPixels;
const pageHeight = coverage.layout.canvasHeightCells * cellHeight;
const pagePixels = new Uint8Array(pageWidth * pageHeight).fill(255);
const page: GrayImage = { pixels: pagePixels, width: pageWidth, height: pageHeight };
let contentPixels = 0;
for (const panel of panels) {
    const expectedHeight = (panel.item.endLine - panel.item.startLine + 1) * cellHeight;
    assert.equal(panel.width, columnWidthPixels, "masonry panel width drifted");
    assert.equal(
        panel.height,
        expectedHeight,
        `masonry panel height drifted for ${panel.item.category}/${panel.item.room ?? "banner"}`,
    );
    blitInk(
        page,
        panel,
        panel.item.column * columnWidthPixels,
        (panel.item.startLine - 1) * cellHeight,
    );
    contentPixels += panel.width * panel.height;
}
assert.equal(droppedChars, 0, "glyph atlas dropped palace characters");
const canvasPixels = page.width * page.height;
assert.ok(contentPixels <= canvasPixels, "masonry panels exceed cropped canvas");
const inkPixels = page.pixels.reduce((sum, value) => sum + (value < 250 ? 1 : 0), 0);

mkdirSync(OUTPUT_DIR, { recursive: true });
for (const file of readdirSync(OUTPUT_DIR)) {
    if (/^palace-page\d+\.png$/.test(file)) unlinkSync(join(OUTPUT_DIR, file));
}
const outputPath = join(OUTPUT_DIR, "palace-page1.png");
await Bun.write(outputPath, await encodeGrayPng(page.pixels, page.width, page.height));

const imageTokens = Math.ceil(canvasPixels / 750);
const proseTextTokens = source.length / 4;
const palaceTextTokens = palace.length / 4;
const report = {
    chars: palace.length,
    pages: 1,
    droppedChars,
    imageTokens,
    textTokenEquivalent: Number(palaceTextTokens.toFixed(2)),
    proseTextTokens: Number(proseTextTokens.toFixed(2)),
    compressionRatios: {
        versusProseAsText: ratio(proseTextTokens, imageTokens),
        versusProseAsImage: ratio(PROSE_IMAGE_BASELINE_TOKENS, imageTokens),
    },
    utilization: {
        contentPixels,
        canvasPixels,
        contentToCanvasRatio: Number((contentPixels / canvasPixels).toFixed(4)),
        contentToCanvasPercent: Number(((contentPixels / canvasPixels) * 100).toFixed(2)),
        inkPixels,
        inkToCanvasPercent: Number(((inkPixels / canvasPixels) * 100).toFixed(2)),
    },
    composition: {
        approach: "per-room grayscale renders with nearest-neighbor 2x title overlays",
        columns: coverage.layout.columns,
        columnWidthChars: coverage.layout.roomWidthChars,
        columnWidthPixels,
        titleScale: TITLE_SCALE,
    },
    coverage: {
        sourceMemories: coverage.sourceMemoryCount,
        entries: coverage.entryCount,
        merges: coverage.mergeCount,
        representedMemories: coverage.representedMemoryCount,
    },
    pagesDetail: [
        {
            page: 1,
            path: outputPath,
            width: page.width,
            height: page.height,
            imageTokens,
        },
    ],
    rooms: coverage.rooms.map((room) => ({
        category: room.category,
        name: room.name,
        entries: room.entryCount,
        merges: room.mergeCount,
        memories: room.memoryCount,
        column: room.column,
    })),
};
console.log(`chars=${report.chars}`);
console.log(`pages=${report.pages}`);
console.log(`droppedChars=${report.droppedChars}`);
console.log(`imageTokens=${report.imageTokens}`);
console.log(`textTokenEquivalent=${report.textTokenEquivalent}`);
console.log(`contentToCanvas=${report.utilization.contentToCanvasPercent}%`);
console.log(`inkToCanvas=${report.utilization.inkToCanvasPercent}%`);
console.log(`versusProseAsText=${report.compressionRatios.versusProseAsText}x`);
console.log(`versusProseAsImage=${report.compressionRatios.versusProseAsImage}x`);
console.log(`RESULT_JSON=${JSON.stringify(report)}`);
