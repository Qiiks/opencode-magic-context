import { strict as assert } from "node:assert";
import { mkdirSync, readdirSync, readFileSync, unlinkSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { renderTextToImages } from "/Users/ufukaltinok/Work/OSS/pxpipe/src/core/library.ts";
import { encodeRgbPng } from "/Users/ufukaltinok/Work/OSS/pxpipe/src/core/png.ts";
import {
    PAD_X,
    PAD_Y,
    renderCellHeight,
    renderCellWidth,
} from "/Users/ufukaltinok/Work/OSS/pxpipe/src/core/render.ts";

const HERE = dirname(fileURLToPath(import.meta.url));
const SOURCE_PATH = "/tmp/visual-memory/trimmed-memories-source.txt";
const OUTPUT_DIR = "/tmp/visual-memory";
const MAX_PALACE_CHARS = 70_000;
const JETBRAINS_VARIANT = process.env.PALACE_RENDER_FONT === "jetbrains-mono-10";
const RENDER_FONT = JETBRAINS_VARIANT ? "jetbrains-mono-10" : "spleen-5x8";
const RENDER_STYLE = { aa: true, font: RENDER_FONT } as const;
const PALACE_PATH = JETBRAINS_VARIANT
    ? "/tmp/visual-memory/palace-jb-layout.txt"
    : join(HERE, "palace.txt");
const COVERAGE_PATH = JETBRAINS_VARIANT
    ? "/tmp/visual-memory/coverage-jb-layout.json"
    : join(HERE, "coverage.json");
const OUTPUT_PREFIX = JETBRAINS_VARIANT ? "palace-jb-page" : "palace-page";
const TITLE_SCALE = 2;
const PATCH_SIZE = 28;
const PAGE_WIDTH_PIXELS = 1_092;
const PAGE_HEIGHT_PIXELS = 1_092;
const BODY_INK = [18, 20, 24] as const;
const PROHIBITION_INK = [148, 28, 35] as const;
let extendedBorderPixels = 0;
const CATEGORY_INK: Record<string, readonly [number, number, number]> = {
    PROJECT_RULES: [24, 58, 112],
    ARCHITECTURE: [0, 88, 92],
    CONSTRAINTS: [126, 76, 16],
    CONFIG_VALUES: [88, 52, 132],
    NAMING: [28, 98, 58],
    KNOWN_ISSUES: [72, 78, 86],
};

type LayoutItem = {
    kind: "category" | "room";
    category: string;
    categories?: string[];
    room?: string;
    column: number;
    startLine: number;
    endLine: number;
    page: number;
    pageLine: number;
    pageTopPixels: number;
    heightPixels: number;
    continuation?: boolean;
    segment?: number;
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
    sharedPairCount: number;
    continuation: boolean;
    segment: number;
    page: number;
    pageLine: number;
    pageTopPixels: number;
    heightPixels: number;
};
type Coverage = {
    sourceMemoryCount: number;
    entryCount: number;
    mergeCount: number;
    representedMemoryCount: number;
    palaceChars: number;
    maxLineChars: number;
    layout: {
        font: string;
        cellWidth: number;
        cellHeight: number;
        columns: number;
        roomWidthChars: number;
        pageWidthChars: number;
        columnGapChars: number;
        canvasHeightCells: number;
        pageHeightPixels: number;
        bodyLinePitch: number;
        pages: Array<{
            page: number;
            startLine: number;
            endLine: number;
            heightCells: number;
            heightPixels: number;
        }>;
        cueLengthDistribution: {
            min: number;
            p25: number;
            median: number;
            p75: number;
            p90: number;
            max: number;
        };
        sharedPairCount: number;
        bandGapRowsBefore: number;
        bandGapRowsAfter: number;
        roomSplitCount: number;
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
            page: number;
            pageLine: number;
            mergedInto?: number;
        }
    >;
};
type GrayImage = { pixels: Uint8Array; width: number; height: number };
type RgbImage = { pixels: Uint8Array; width: number; height: number };
type Panel = GrayImage & {
    droppedChars: number;
    item: LayoutItem;
    title?: GrayImage;
    redCells: Set<string>;
    lineTops: number[];
    bodyRows: Set<number>;
    composedHeight: number;
};

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

function prohibitionCells(lines: string[], context: string): Set<string> {
    const cells: Array<{ character: string; row: number; column: number }> = [];
    for (const [row, line] of lines.entries()) {
        for (const [column, character] of [...line].entries())
            cells.push({ character, row, column });
    }
    const entryStarts = cells
        .map((cell, index) => ({ cell, index }))
        .filter(({ cell }) => cell.character === "•")
        .map(({ index }) => index);
    const red = new Set<string>();
    for (const [entryIndex, start] of entryStarts.entries()) {
        const end = entryStarts[entryIndex + 1] ?? cells.length;
        for (let index = start; index < end; index++) {
            if (cells[index]?.character !== "⊘") continue;
            const marker = cells[index];
            if (!marker) continue;
            red.add(`${marker.row}:${marker.column}`);
            let open = -1;
            for (let cursor = index + 1; cursor < end; cursor++) {
                const character = cells[cursor]?.character;
                if (character === "⊘") break;
                if (character === "(") {
                    open = cursor;
                    break;
                }
            }
            assert.ok(
                open >= 0,
                `${context}: prohibition at row ${marker.row + 1} lacks following mechanism: ${lines[marker.row] ?? ""}`,
            );
            let depth = 0;
            let close = -1;
            for (let cursor = open; cursor < end; cursor++) {
                const character = cells[cursor]?.character;
                if (character === "(") depth++;
                if (character === ")") depth--;
                if (depth === 0) {
                    close = cursor;
                    break;
                }
            }
            assert.ok(close >= open, `prohibition at row ${marker.row + 1} has unclosed mechanism`);
            for (let cursor = open; cursor <= close; cursor++) {
                const cell = cells[cursor];
                const lineWidth = cell ? [...(lines[cell.row] ?? "")].length : 0;
                if (
                    cell &&
                    cell.character !== " " &&
                    cell.column > 0 &&
                    cell.column < lineWidth - 1
                ) {
                    red.add(`${cell.row}:${cell.column}`);
                }
            }
            index = close;
        }
    }
    return red;
}

function blendInk(background: number, ink: number, gray: number): number {
    const coverage = 255 - gray;
    return Math.round(background - (coverage * (background - ink)) / 255);
}

function blitColoredInk(
    target: RgbImage,
    source: GrayImage,
    left: number,
    top: number,
    color: readonly [number, number, number],
): void {
    assert.ok(left >= 0 && top >= 0, "negative blit offset");
    assert.ok(
        left + source.width <= target.width && top + source.height <= target.height,
        "blit exceeds target",
    );
    for (let y = 0; y < source.height; y++) {
        for (let x = 0; x < source.width; x++) {
            const gray = source.pixels[y * source.width + x] ?? 255;
            if (gray === 255) continue;
            const index = ((top + y) * target.width + left + x) * 3;
            for (let channel = 0; channel < 3; channel++) {
                target.pixels[index + channel] = blendInk(
                    target.pixels[index + channel] ?? 255,
                    color[channel] ?? 0,
                    gray,
                );
            }
        }
    }
}

function blitPanel(
    target: RgbImage,
    panel: Panel,
    left: number,
    top: number,
    cellWidth: number,
    cellHeight: number,
): void {
    const baseCategoryColor = CATEGORY_INK[panel.item.category];
    assert.ok(baseCategoryColor, `missing category color for ${panel.item.category}`);
    const rows = panel.height / cellHeight;
    const columns = panel.width / cellWidth;
    const colorFor = (
        row: number,
        column: number,
        red: boolean,
    ): readonly [number, number, number] => {
        const border =
            panel.item.kind === "category" ||
            row === 0 ||
            row === rows - 1 ||
            column === 0 ||
            column === columns - 1;
        const bannerCategories = panel.item.categories ?? [panel.item.category];
        const bannerCategory =
            bannerCategories.length > 1 && column >= columns / 2
                ? bannerCategories.at(-1)
                : bannerCategories[0];
        const categoryColor =
            CATEGORY_INK[bannerCategory ?? panel.item.category] ?? baseCategoryColor;
        return red ? PROHIBITION_INK : border ? categoryColor : BODY_INK;
    };
    for (let y = 0; y < panel.height; y++) {
        const row = Math.floor(y / cellHeight);
        const rowTop = panel.lineTops[row];
        if (rowTop === undefined) continue;
        const targetY = top + rowTop + (y % cellHeight);
        for (let x = 0; x < panel.width; x++) {
            const gray = panel.pixels[y * panel.width + x] ?? 255;
            if (gray === 255) continue;
            const column = Math.floor(x / cellWidth);
            const red = panel.redCells.has(`${row}:${column}`);
            const color = colorFor(row, column, red);
            const index = (targetY * target.width + left + x) * 3;
            for (let channel = 0; channel < 3; channel++) {
                target.pixels[index + channel] = blendInk(
                    target.pixels[index + channel] ?? 255,
                    color[channel] ?? 0,
                    gray,
                );
            }
        }
    }
    for (const row of panel.bodyRows) {
        const rowTop = panel.lineTops[row];
        const nextTop = panel.lineTops[row + 1];
        if (rowTop === undefined || nextTop === undefined || nextTop <= rowTop + cellHeight)
            continue;
        for (const column of [0, columns - 1]) {
            const xStart = column * cellWidth;
            for (let xOffset = 0; xOffset < cellWidth; xOffset++) {
                let gray = 255;
                for (let sourceY = row * cellHeight; sourceY < (row + 1) * cellHeight; sourceY++) {
                    gray = Math.min(
                        gray,
                        panel.pixels[sourceY * panel.width + xStart + xOffset] ?? 255,
                    );
                }
                if (gray === 255) continue;
                for (let targetY = top + rowTop + cellHeight; targetY < top + nextTop; targetY++) {
                    extendedBorderPixels++;
                    const index = (targetY * target.width + left + xStart + xOffset) * 3;
                    for (let channel = 0; channel < 3; channel++) {
                        target.pixels[index + channel] = blendInk(
                            target.pixels[index + channel] ?? 255,
                            baseCategoryColor[channel] ?? 0,
                            gray,
                        );
                    }
                }
            }
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

function panelGeometry(
    item: LayoutItem,
    lineCount: number,
    cellHeight: number,
    bodyLinePitch: number,
): { lineTops: number[]; bodyRows: Set<number>; composedHeight: number } {
    if (item.kind === "category") {
        return { lineTops: [0], bodyRows: new Set(), composedHeight: cellHeight };
    }
    const headerRows = item.continuation ? 1 : 2;
    const bottomRow = lineCount - 1;
    const lineTops: number[] = [];
    const bodyRows = new Set<number>();
    let top = 0;
    for (let row = 0; row < lineCount; row++) {
        lineTops.push(top);
        const body = row >= headerRows && row < bottomRow;
        if (body) bodyRows.add(row);
        top += body ? bodyLinePitch : cellHeight;
    }
    return { lineTops, bodyRows, composedHeight: top };
}

function extractItemLines(palaceLines: string[], item: LayoutItem, coverage: Coverage): string[] {
    if (item.kind === "category") {
        return [
            [...(palaceLines[item.startLine - 1] ?? "")]
                .slice(0, coverage.layout.pageWidthChars)
                .join(""),
        ];
    }
    const left = item.column * (coverage.layout.roomWidthChars + coverage.layout.columnGapChars);
    const lines: string[] = [];
    for (let lineNumber = item.startLine; lineNumber <= item.endLine; lineNumber++) {
        const source = [...(palaceLines[lineNumber - 1] ?? "")];
        lines.push(source.slice(left, left + coverage.layout.roomWidthChars).join(""));
    }
    return lines;
}

const palace = readFileSync(PALACE_PATH, "utf8");
const coverage = JSON.parse(readFileSync(COVERAGE_PATH, "utf8")) as Coverage;
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
    coverage.rooms.map((room) => [`${room.category}\u0000${room.name}\u0000${room.segment}`, room]),
);
const panels: Panel[] = [];
let droppedChars = 0;
for (const item of coverage.layout.items) {
    const lines = extractItemLines(palaceLines, item, coverage);
    if (item.kind === "room") {
        const room = roomByKey.get(
            `${item.category}\u0000${item.room ?? ""}\u0000${item.segment ?? 0}`,
        );
        assert.ok(room, `coverage room missing for ${item.category}/${item.room}`);
        assert.ok(
            lines.length >= (item.continuation ? 2 : 3),
            `room ${room.name} lacks frame rows`,
        );
        const redCells = prohibitionCells(lines, `${item.category}/${room.name}`);
        let scaledTitle: GrayImage | undefined;
        let titleDroppedChars = 0;
        if (!item.continuation) {
            const title = await renderPanelText(room.name, [...room.name].length);
            scaledTitle = upscale2x(title);
            titleDroppedChars = title.droppedChars;
            const titleCells = Math.ceil(scaledTitle.width / renderCellWidth(RENDER_STYLE));
            const titleStart = Math.floor((coverage.layout.roomWidthChars - titleCells) / 2);
            const top = [...(lines[0] ?? "")];
            for (let column = titleStart; column < titleStart + titleCells; column++) {
                if (column > 0 && column < coverage.layout.roomWidthChars - 1) top[column] = " ";
            }
            lines[0] = top.join("");
            const side = [...(lines[1] ?? "")][0] ?? "│";
            lines[1] = `${side}${" ".repeat(coverage.layout.roomWidthChars - 2)}${side}`;
        }
        const panel = await renderPanelText(lines.join("\n"), coverage.layout.roomWidthChars);
        const geometry = panelGeometry(
            item,
            lines.length,
            renderCellHeight(RENDER_STYLE),
            coverage.layout.bodyLinePitch,
        );
        droppedChars += panel.droppedChars + titleDroppedChars;
        panels.push({
            ...panel,
            ...geometry,
            droppedChars: panel.droppedChars + titleDroppedChars,
            item,
            ...(scaledTitle ? { title: scaledTitle } : {}),
            redCells,
        });
    } else {
        const panel = await renderPanelText(lines.join("\n"), coverage.layout.pageWidthChars);
        droppedChars += panel.droppedChars;
        panels.push({
            ...panel,
            ...panelGeometry(
                item,
                lines.length,
                renderCellHeight(RENDER_STYLE),
                coverage.layout.bodyLinePitch,
            ),
            droppedChars: panel.droppedChars,
            item,
            redCells: new Set(),
        });
    }
}

const cellWidth = renderCellWidth(RENDER_STYLE);
const cellHeight = renderCellHeight(RENDER_STYLE);
const measuredColumns = Math.floor(PAGE_WIDTH_PIXELS / cellWidth);
const measuredRows = Math.floor(PAGE_HEIGHT_PIXELS / coverage.layout.bodyLinePitch);
assert.equal(RENDER_FONT, coverage.layout.font, "author font layout drifted");
assert.equal(cellWidth, coverage.layout.cellWidth, "author cell width drifted");
assert.equal(cellHeight, coverage.layout.cellHeight, "author cell height drifted");
assert.equal(
    cellHeight + 1,
    coverage.layout.bodyLinePitch,
    "body leading must add exactly one pixel",
);
assert.ok(
    coverage.layout.pageWidthChars <= measuredColumns,
    "author page width exceeds atlas capacity",
);
const columnWidthPixels = coverage.layout.roomWidthChars * cellWidth;
const contentWidthPixels = coverage.layout.pageWidthChars * cellWidth;
const pageLeft = Math.floor((PAGE_WIDTH_PIXELS - contentWidthPixels) / 2);
assert.ok(pageLeft >= 0, "palace content exceeds fixed page width");
const pageCanvases = coverage.layout.pages.map((layoutPage, index) => {
    const last = index === coverage.layout.pages.length - 1;
    const usedHeight = layoutPage.heightPixels;
    const height = last
        ? Math.max(PATCH_SIZE, Math.ceil(usedHeight / PATCH_SIZE) * PATCH_SIZE)
        : PAGE_HEIGHT_PIXELS;
    assert.ok(height <= PAGE_HEIGHT_PIXELS, `page ${layoutPage.page} exceeds fixed geometry`);
    return {
        layout: layoutPage,
        image: {
            pixels: new Uint8Array(PAGE_WIDTH_PIXELS * height * 3).fill(255),
            width: PAGE_WIDTH_PIXELS,
            height,
        } satisfies RgbImage,
        contentPixels: 0,
    };
});
for (const panel of panels) {
    const page = pageCanvases[panel.item.page - 1];
    assert.ok(page, `missing canvas for page ${panel.item.page}`);
    const expectedNativeHeight = (panel.item.endLine - panel.item.startLine + 1) * cellHeight;
    const expectedWidth = panel.item.kind === "category" ? contentWidthPixels : columnWidthPixels;
    assert.equal(panel.width, expectedWidth, "masonry panel width drifted");
    assert.equal(
        panel.height,
        expectedNativeHeight,
        `masonry panel height drifted for ${panel.item.category}/${panel.item.room ?? "banner"}`,
    );
    const left = pageLeft + panel.item.column * columnWidthPixels;
    assert.equal(
        panel.composedHeight,
        panel.item.heightPixels,
        "leading-aware panel height drifted",
    );
    const top = panel.item.pageTopPixels;
    assert.ok(
        top + panel.composedHeight <= page.image.height,
        `panel exceeds page ${panel.item.page}`,
    );
    blitPanel(page.image, panel, left, top, cellWidth, cellHeight);
    if (panel.title) {
        const titleLeft = left + Math.floor((panel.width - panel.title.width) / 2);
        const titleColor = CATEGORY_INK[panel.item.category];
        assert.ok(titleColor, `missing title color for ${panel.item.category}`);
        blitColoredInk(page.image, panel.title, titleLeft, top, titleColor);
    }
    page.contentPixels += panel.width * panel.composedHeight;
}
assert.equal(droppedChars, 0, "glyph atlas dropped palace characters");
assert.ok(extendedBorderPixels > 0, "body leading did not extend vertical room borders");
const prohibitionCellCount = panels.reduce((total, panel) => total + panel.redCells.size, 0);
assert.ok(prohibitionCellCount > 0, "prohibition palette was not exercised");

mkdirSync(OUTPUT_DIR, { recursive: true });
for (const file of readdirSync(OUTPUT_DIR)) {
    if (new RegExp(`^${OUTPUT_PREFIX}\\d+\\.png$`).test(file)) unlinkSync(join(OUTPUT_DIR, file));
}
const patchTokens = (width: number, height: number): number =>
    Math.ceil(width / PATCH_SIZE) * Math.ceil(height / PATCH_SIZE);
const pagesDetail = [];
let imageTokens = 0;
let canvasPixels = 0;
let contentPixels = 0;
let inkPixels = 0;
for (const page of pageCanvases) {
    let pageInkPixels = 0;
    for (let index = 0; index < page.image.pixels.length; index += 3) {
        if (
            (page.image.pixels[index] ?? 255) < 250 ||
            (page.image.pixels[index + 1] ?? 255) < 250 ||
            (page.image.pixels[index + 2] ?? 255) < 250
        ) {
            pageInkPixels++;
        }
    }
    const cellColumns = Math.floor(page.image.width / cellWidth);
    const cellRows = Math.floor(page.image.height / coverage.layout.bodyLinePitch);
    const sourcePageLines = palaceLines.slice(page.layout.startLine - 1, page.layout.endLine);
    const nonPadCharacterCells = sourcePageLines.reduce(
        (total, line) => total + [...line].filter((character) => character !== " ").length,
        0,
    );
    const tokens = patchTokens(page.image.width, page.image.height);
    const outputPath = join(OUTPUT_DIR, `${OUTPUT_PREFIX}${page.layout.page}.png`);
    await Bun.write(
        outputPath,
        await encodeRgbPng(page.image.pixels, page.image.width, page.image.height),
    );
    const pageCanvasPixels = page.image.width * page.image.height;
    const totalCharacterCells = cellColumns * cellRows;
    pagesDetail.push({
        page: page.layout.page,
        path: outputPath,
        width: page.image.width,
        height: page.image.height,
        patchColumns: Math.ceil(page.image.width / PATCH_SIZE),
        patchRows: Math.ceil(page.image.height / PATCH_SIZE),
        imageTokens: tokens,
        contentPixels: page.contentPixels,
        canvasPixels: pageCanvasPixels,
        panelFillPercent: Number(((page.contentPixels / pageCanvasPixels) * 100).toFixed(2)),
        nonPadCharacterCells,
        totalCharacterCells,
        fillRatio: Number((nonPadCharacterCells / totalCharacterCells).toFixed(4)),
        fillPercent: Number(((nonPadCharacterCells / totalCharacterCells) * 100).toFixed(2)),
        inkPixels: pageInkPixels,
    });
    imageTokens += tokens;
    canvasPixels += pageCanvasPixels;
    contentPixels += page.contentPixels;
    inkPixels += pageInkPixels;
}
const proseTextTokens = source.length / 4;
const palaceTextTokens = palace.length / 4;
const bodyLeadingPixels = coverage.rooms.reduce(
    (total, room) => total + Math.max(0, room.heightCells - (room.continuation ? 2 : 3)),
    0,
);

let fontComparison: Record<string, unknown> | undefined;
if (!JETBRAINS_VARIANT) {
    const variantEnvironment = { ...process.env, PALACE_LAYOUT_FONT: "jetbrains-mono-10" };
    const authored = Bun.spawnSync({
        cmd: ["bun", join(HERE, "author-palace.ts")],
        cwd: HERE,
        env: variantEnvironment,
        stdout: "pipe",
        stderr: "pipe",
    });
    if (authored.exitCode !== 0) {
        throw new Error(
            `JetBrains layout authoring failed: ${new TextDecoder().decode(authored.stderr)}`,
        );
    }
    const rendered = Bun.spawnSync({
        cmd: ["bun", join(HERE, "build-palace.ts")],
        cwd: HERE,
        env: { ...process.env, PALACE_RENDER_FONT: "jetbrains-mono-10" },
        stdout: "pipe",
        stderr: "pipe",
    });
    if (rendered.exitCode !== 0) {
        throw new Error(
            `JetBrains comparison render failed: ${new TextDecoder().decode(rendered.stderr)}`,
        );
    }
    const variantOutput = new TextDecoder().decode(rendered.stdout);
    const resultLine = variantOutput.match(/^RESULT_JSON=(.+)$/m)?.[1];
    if (!resultLine) throw new Error("JetBrains comparison render omitted RESULT_JSON");
    const variantReport = JSON.parse(resultLine) as {
        pages: number;
        imageTokens: number;
        droppedChars: number;
        utilization: { contentToCanvasPercent: number };
        pagesDetail: unknown[];
    };
    fontComparison = {
        defaultFont: RENDER_FONT,
        defaultTokens: imageTokens,
        jetbrainsFont: "jetbrains-mono-10",
        jetbrainsPages: variantReport.pages,
        jetbrainsTokens: variantReport.imageTokens,
        jetbrainsToSpleenRatio: Number((variantReport.imageTokens / imageTokens).toFixed(3)),
        jetbrainsDroppedChars: variantReport.droppedChars,
        jetbrainsContentToCanvasPercent: variantReport.utilization.contentToCanvasPercent,
        jetbrainsPagesDetail: variantReport.pagesDetail,
    };
}

const report = {
    chars: palace.length,
    pages: pagesDetail.length,
    droppedChars,
    imageTokens,
    textTokenEquivalent: Number(palaceTextTokens.toFixed(2)),
    proseTextTokens: Number(proseTextTokens.toFixed(2)),
    compressionRatios: {
        versusProseAsText: ratio(proseTextTokens, imageTokens),
    },
    utilization: {
        contentPixels,
        canvasPixels,
        contentToCanvasRatio: Number((contentPixels / canvasPixels).toFixed(4)),
        contentToCanvasPercent: Number(((contentPixels / canvasPixels) * 100).toFixed(2)),
        inkPixels,
        inkToCanvasPercent: Number(((inkPixels / canvasPixels) * 100).toFixed(2)),
        pages: pagesDetail.map((page) => ({
            page: page.page,
            fillRatio: page.fillRatio,
            fillPercent: page.fillPercent,
            panelFillPercent: page.panelFillPercent,
        })),
    },
    composition: {
        approach:
            "category-band masonry composed from grayscale room renders into deterministic RGB; nearest-neighbor 2x title overlays",
        font: RENDER_FONT,
        columns: coverage.layout.columns,
        columnWidthChars: coverage.layout.roomWidthChars,
        columnWidthPixels,
        measuredAtlasCell: { width: cellWidth, height: cellHeight },
        measuredPageCapacity: { columns: measuredColumns, rows: measuredRows },
        pageGeometry: {
            width: PAGE_WIDTH_PIXELS,
            fullHeight: PAGE_HEIGHT_PIXELS,
            patchSize: PATCH_SIZE,
        },
        cueLengthDistribution: coverage.layout.cueLengthDistribution,
        sharedPairCount: coverage.layout.sharedPairCount,
        bodyLinePitch: coverage.layout.bodyLinePitch,
        addedLeadingPixelsAcrossRoomSegments: bodyLeadingPixels,
        verticalBordersExtendedAcrossLeading: true,
        extendedBorderPixels,
        leveling: {
            gapRowsBefore: coverage.layout.bandGapRowsBefore,
            gapRowsAfter: coverage.layout.bandGapRowsAfter,
            roomSplits: coverage.layout.roomSplitCount,
        },
        titleScale: TITLE_SCALE,
        palette: CATEGORY_INK,
        prohibitionInk: PROHIBITION_INK,
        prohibitionCellCount,
        bodyInk: BODY_INK,
    },
    coverage: {
        sourceMemories: coverage.sourceMemoryCount,
        entries: coverage.entryCount,
        merges: coverage.mergeCount,
        representedMemories: coverage.representedMemoryCount,
    },
    pagesDetail,
    ...(fontComparison ? { fontComparison } : {}),
    rooms: coverage.rooms.map((room) => ({
        category: room.category,
        name: room.name,
        entries: room.entryCount,
        merges: room.mergeCount,
        memories: room.memoryCount,
        page: room.page,
        column: room.column,
        sharedPairs: room.sharedPairCount,
    })),
};
console.log(`chars=${report.chars}`);
console.log(`pages=${report.pages}`);
console.log(`droppedChars=${report.droppedChars}`);
console.log(`imageTokens=${report.imageTokens}`);
console.log(`textTokenEquivalent=${report.textTokenEquivalent}`);
console.log(`contentToCanvas=${report.utilization.contentToCanvasPercent}%`);
console.log(`inkToCanvas=${report.utilization.inkToCanvasPercent}%`);
for (const page of report.pagesDetail) {
    console.log(
        `page${page.page}=${page.width}x${page.height} patches=${page.patchColumns}x${page.patchRows} tokens=${page.imageTokens} fill=${page.fillPercent}%`,
    );
}
console.log(`versusProseAsText=${report.compressionRatios.versusProseAsText}x`);
console.log(`RESULT_JSON=${JSON.stringify(report)}`);
