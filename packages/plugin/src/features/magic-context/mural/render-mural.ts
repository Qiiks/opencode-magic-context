import { deflateSync } from "node:zlib";

/** Maximum canvas extent; sparse renders use only the needed snapped extent. */
export const MURAL_WIDTH = 1_092;
export const MURAL_HEIGHT = 1_092;
/** Anthropic vision image-token tiles are 28 pixels on each side. */
export const MURAL_VISION_TILE = 28;
export const MURAL_FONT = "spleen-5x8";
export const MURAL_CELL_WIDTH = 5;
export const MURAL_CELL_HEIGHT = 8;
export const MURAL_LINE_PITCH = 9;
export const MURAL_COLUMNS = 3;
export const MURAL_COLUMN_GAP = 1;
/** Character width of one column. High-importance cues can be up to 90 chars, so
 *  a cue wider than this wraps across lines (word-wrap) rather than truncating —
 *  this restores the true three-column fill the single-column port lost. */
export const MURAL_ROOM_WIDTH = 72;
export const MURAL_ROWS = Math.floor(MURAL_HEIGHT / MURAL_LINE_PITCH);
export const MURAL_LINE_CAPACITY = MURAL_COLUMNS * MURAL_ROWS;

export type MuralCategory = string;

/** A flat mural entry to render. No rooms, no merges — resolveMural produces a
 *  pre-ordered flat list (category band → importance DESC → id ASC) and the
 *  renderer packs it deterministically into the capped image. */
export interface MuralRenderEntry {
    id: number;
    category: MuralCategory;
    importance: number;
    cue: string;
}

export interface MuralLayoutItem {
    kind: "category" | "entry";
    category: MuralCategory;
    column: number;
    startLine: number;
    endLine: number;
}

export interface MuralRenderResult {
    png: Uint8Array;
    dataUrl: string;
    muralText: string;
    sha256Input: string;
    placements: Map<number, { category: MuralCategory; column: number; line: number }>;
    layoutItems: MuralLayoutItem[];
    renderedIds: number[];
    /** Entries trimmed because the capped image filled before reaching them. */
    droppedIds: number[];
    categoryLineUsage: Record<string, number>;
    /** Content lines actually placed in the grid (excludes blank cells). Used to
     *  assert the three-column fill occupancy. */
    filledLineCount: number;
    /** PNG dimensions after content cropping and vision-tile snapping. */
    width: number;
    height: number;
}

type Cursor = { column: number; row: number };

const CATEGORY_COLORS: Record<string, readonly [number, number, number]> = {
    PROJECT_RULES: [24, 58, 112],
    ARCHITECTURE: [0, 88, 92],
    CONSTRAINTS: [126, 76, 16],
    CONFIG_VALUES: [88, 52, 132],
    NAMING: [28, 98, 58],
};
const BODY_INK: readonly [number, number, number] = [18, 20, 24];
const PROHIBITION_INK: readonly [number, number, number] = [148, 28, 35];

function codepoints(value: string): number {
    return [...value].length;
}

function escapeText(value: string): string {
    return value
        .replace(/[\r\n]+/g, " ")
        .replace(/\s+/g, " ")
        .trim();
}

function banner(category: MuralCategory): string {
    const label = ` <${category}> `;
    if (codepoints(label) > MURAL_ROOM_WIDTH) {
        // A category name longer than a column is degenerate; hard-truncate the
        // banner rather than throw — the deterministic renderer must never fail
        // the m0 injection over a label width.
        return label.slice(0, MURAL_ROOM_WIDTH);
    }
    const remaining = MURAL_ROOM_WIDTH - codepoints(label);
    return `${"─".repeat(Math.floor(remaining / 2))}${label}${"─".repeat(Math.ceil(remaining / 2))}`;
}

/** Hard-break a single token wider than the column into column-width slices, so
 *  a long verbatim path/hash in a cue can never overrun a line (word-wrap alone
 *  can't split an unbreakable token). */
function breakLongToken(token: string, width: number): string[] {
    const chars = [...token];
    if (chars.length <= width) return [token];
    const slices: string[] = [];
    for (let i = 0; i < chars.length; i += width) {
        slices.push(chars.slice(i, i + width).join(""));
    }
    return slices;
}

/**
 * Word-wrap one cue into bullet lines that fit the column width. The first line
 * is bulleted (`•`), continuations are indented one space. Shape adapted from
 * the visual-memory experiment's appendEntry, hardened to hard-break an
 * over-wide single token instead of throwing.
 */
function wrapCue(cue: string, width: number): string[] {
    const words = escapeText(cue)
        .split(/\s+/)
        .filter(Boolean)
        .flatMap((word) => breakLongToken(word, width - 1));
    if (words.length === 0) return ["•"];
    const lines: string[] = [];
    let line = "•";
    for (const word of words) {
        const separator = line === "•" || line === " " ? "" : " ";
        const candidate = `${line}${separator}${word}`;
        if (codepoints(candidate) <= width) {
            line = candidate;
            continue;
        }
        lines.push(line);
        line = ` ${word}`;
    }
    lines.push(line);
    return lines;
}

interface PlannedLine {
    text: string;
    /** Entry ids whose body starts on this line (two for a shared pair). */
    entryIds: number[];
    isBanner: boolean;
    category: MuralCategory;
}

/**
 * Build the flat line plan for the pre-ordered entries: a category banner at
 * each band boundary, then the entries' cue lines with shared-pair packing (two
 * short non-prohibition cues on one line — a density win) and word-wrap for the
 * rest.
 */
function planLines(entries: readonly MuralRenderEntry[]): PlannedLine[] {
    const lines: PlannedLine[] = [];
    // Two short cues share a line as `•a • b`; each half gets at most half the
    // column minus the bullets/separator overhead.
    const shortEntryLimit = Math.floor((MURAL_ROOM_WIDTH - 4) / 2);

    let currentCategory: string | null = null;
    for (let index = 0; index < entries.length; index++) {
        const entry = entries[index]!;
        if (entry.category !== currentCategory) {
            currentCategory = entry.category;
            lines.push({
                text: banner(entry.category),
                entryIds: [],
                isBanner: true,
                category: entry.category,
            });
        }

        const cue = escapeText(entry.cue);
        const next = entries[index + 1];
        const sameCategoryNext = next && next.category === entry.category;
        const nextCue = sameCategoryNext ? escapeText(next.cue) : "";
        const shared = `•${cue} • ${nextCue}`;
        if (
            sameCategoryNext &&
            !cue.includes("⊘") &&
            !nextCue.includes("⊘") &&
            codepoints(cue) <= shortEntryLimit &&
            codepoints(nextCue) <= shortEntryLimit &&
            codepoints(shared) <= MURAL_ROOM_WIDTH
        ) {
            lines.push({
                text: shared,
                entryIds: [entry.id, next.id],
                isBanner: false,
                category: entry.category,
            });
            index++; // consumed the pair
            continue;
        }

        const wrapped = wrapCue(cue, MURAL_ROOM_WIDTH);
        wrapped.forEach((text, wrappedIndex) => {
            lines.push({
                text,
                // Only the first wrapped line anchors the entry's placement.
                entryIds: wrappedIndex === 0 ? [entry.id] : [],
                isBanner: false,
                category: entry.category,
            });
        });
    }
    return lines;
}

function advance(cursor: Cursor): Cursor {
    if (cursor.row + 1 < MURAL_ROWS) return { column: cursor.column, row: cursor.row + 1 };
    return { column: cursor.column + 1, row: 0 };
}

function atEnd(cursor: Cursor): boolean {
    return cursor.column >= MURAL_COLUMNS;
}

interface LayoutResult {
    text: string;
    grid: string[][];
    placements: MuralRenderResult["placements"];
    layoutItems: MuralLayoutItem[];
    renderedIds: number[];
    droppedIds: number[];
    usage: Record<string, number>;
    filledLineCount: number;
    columnCount: number;
    rowCount: number;
}

function renderLayout(entries: readonly MuralRenderEntry[]): LayoutResult {
    const plan = planLines(entries);
    const grid = Array.from({ length: MURAL_COLUMNS }, () =>
        Array.from({ length: MURAL_ROWS }, () => ""),
    );
    const placements: MuralRenderResult["placements"] = new Map();
    const layoutItems: MuralLayoutItem[] = [];
    const renderedIds: number[] = [];
    const placedIds = new Set<number>();
    const usage: Record<string, number> = {};
    let cursor: Cursor = { column: 0, row: 0 };
    let filledLineCount = 0;

    for (const line of plan) {
        if (atEnd(cursor)) break;
        // Never strand a category banner on the last row of a column — push it to
        // the next column so its band isn't orphaned (mirrors the old room-header
        // rule). A body line may take the last row.
        if (line.isBanner && cursor.row === MURAL_ROWS - 1) {
            cursor = advance(cursor);
            if (atEnd(cursor)) break;
        }

        grid[cursor.column]![cursor.row] = line.text;
        filledLineCount += 1;
        usage[line.category] = (usage[line.category] ?? 0) + 1;
        const placementLine = cursor.row + 1;
        const placementColumn = cursor.column;
        if (line.isBanner) {
            layoutItems.push({
                kind: "category",
                category: line.category,
                column: placementColumn,
                startLine: placementLine,
                endLine: placementLine,
            });
        }
        for (const id of line.entryIds) {
            placements.set(id, {
                category: line.category,
                column: placementColumn,
                line: placementLine,
            });
            if (!placedIds.has(id)) {
                placedIds.add(id);
                renderedIds.push(id);
            }
            layoutItems.push({
                kind: "entry",
                category: line.category,
                column: placementColumn,
                startLine: placementLine,
                endLine: placementLine,
            });
        }
        cursor = advance(cursor);
    }

    const droppedIds = entries.filter((entry) => !placedIds.has(entry.id)).map((entry) => entry.id);

    let columnCount = 0;
    let rowCount = 0;
    for (let column = 0; column < MURAL_COLUMNS; column++) {
        for (let row = 0; row < MURAL_ROWS; row++) {
            if (!grid[column]?.[row]) continue;
            columnCount = Math.max(columnCount, column + 1);
            rowCount = Math.max(rowCount, row + 1);
        }
    }
    const textLines = Array.from({ length: rowCount }, (_, row) =>
        Array.from({ length: columnCount }, (_, column) =>
            (grid[column]?.[row] ?? "").padEnd(MURAL_ROOM_WIDTH),
        ).join(" "),
    );
    return {
        text: `${textLines.join("\n")}\n`,
        grid,
        placements,
        layoutItems,
        renderedIds,
        droppedIds,
        usage,
        filledLineCount,
        columnCount,
        rowCount,
    };
}

function crc32(bytes: Uint8Array): number {
    let crc = 0xffffffff;
    for (const byte of bytes) {
        crc ^= byte;
        for (let bit = 0; bit < 8; bit++) crc = (crc >>> 1) ^ (0xedb88320 & -(crc & 1));
    }
    return (crc ^ 0xffffffff) >>> 0;
}

function pngChunk(type: string, data: Uint8Array): Uint8Array {
    const typeBytes = new TextEncoder().encode(type);
    const output = new Uint8Array(12 + data.length);
    const view = new DataView(output.buffer);
    view.setUint32(0, data.length);
    output.set(typeBytes, 4);
    output.set(data, 8);
    view.setUint32(8 + data.length, crc32(output.subarray(4, 8 + data.length)));
    return output;
}

function encodeRgbPng(pixels: Uint8Array, width: number, height: number): Uint8Array {
    const raw = new Uint8Array((width * 3 + 1) * height);
    for (let y = 0; y < height; y++) {
        const rawStart = y * (width * 3 + 1);
        raw[rawStart] = 0;
        raw.set(pixels.subarray(y * width * 3, (y + 1) * width * 3), rawStart + 1);
    }
    const ihdr = new Uint8Array(13);
    const header = new DataView(ihdr.buffer);
    header.setUint32(0, width);
    header.setUint32(4, height);
    ihdr[8] = 8;
    ihdr[9] = 2;
    const signature = new Uint8Array([137, 80, 78, 71, 13, 10, 26, 10]);
    const compressed = new Uint8Array(deflateSync(raw, { level: 9 }));
    const chunks = [
        signature,
        pngChunk("IHDR", ihdr),
        pngChunk("IDAT", compressed),
        pngChunk("IEND", new Uint8Array()),
    ];
    const output = new Uint8Array(chunks.reduce((total, chunk) => total + chunk.length, 0));
    let offset = 0;
    for (const chunk of chunks) {
        output.set(chunk, offset);
        offset += chunk.length;
    }
    return output;
}

// Spleen's 5x8 grid is intentionally represented as a stable, tiny atlas. The
// fallback glyph keeps punctuation and non-ASCII relation marks visible rather
// than silently changing the layout when a cue uses a new symbol.
const GLYPHS: Record<string, readonly string[]> = {
    " ": ["     ", "     ", "     ", "     ", "     ", "     ", "     ", "     "],
    "-": ["     ", "     ", "     ", "     ", "     ", "     ", "     ", "█████"],
    ".": ["     ", "     ", "     ", "     ", "     ", "     ", " ██  ", " ██  "],
    ":": ["     ", " ██  ", " ██  ", "     ", "     ", " ██  ", " ██  ", "     "],
    "/": ["    █", "   █ ", "   █ ", "  █  ", " █   ", " █   ", "█    ", "     "],
    "<": ["     ", "  █  ", " █   ", "█    ", " █   ", "  █  ", "     ", "     "],
    ">": ["     ", "  █  ", "   █ ", "    █", "   █ ", "  █  ", "     ", "     "],
    "→": ["     ", "     ", "  █  ", " ████", "█████", " ████", "  █  ", "     "],
    "←": ["     ", "     ", "  █  ", "█████", " ████", "█████", "  █  ", "     "],
    "⊘": [" ███ ", "█   █", "█ █ █", "██ ██", "██ ██", "█ █ █", "█   █", " ███ "],
    "•": ["     ", "     ", " ██  ", "████ ", "████ ", " ██  ", "     ", "     "],
    "▰": ["█████", "█████", "█████", "█████", "█████", "█████", "█████", "█████"],
    "─": ["     ", "     ", "     ", "     ", "     ", "     ", "     ", "█████"],
    "—": ["     ", "     ", "     ", "     ", "     ", "     ", "█████", "     "],
};

const LETTERS = [
    ["01110", "10001", "10001", "11111", "10001", "10001", "10001"],
    ["11110", "10001", "10001", "11110", "10001", "10001", "11110"],
    ["01111", "10000", "10000", "10000", "10000", "10000", "01111"],
    ["11110", "10001", "10001", "10001", "10001", "10001", "11110"],
    ["11111", "10000", "10000", "11110", "10000", "10000", "11111"],
    ["11111", "10000", "10000", "11110", "10000", "10000", "10000"],
    ["01111", "10000", "10000", "10111", "10001", "10001", "01111"],
    ["10001", "10001", "10001", "11111", "10001", "10001", "10001"],
    ["11111", "00100", "00100", "00100", "00100", "00100", "11111"],
    ["00111", "00010", "00010", "00010", "10010", "10010", "01100"],
    ["10001", "10010", "10100", "11000", "10100", "10010", "10001"],
    ["10000", "10000", "10000", "10000", "10000", "10000", "11111"],
    ["10001", "11011", "10101", "10101", "10001", "10001", "10001"],
    ["10001", "11001", "10101", "10011", "10001", "10001", "10001"],
    ["01110", "10001", "10001", "10001", "10001", "10001", "01110"],
    ["11110", "10001", "10001", "11110", "10000", "10000", "10000"],
    ["01110", "10001", "10001", "10001", "10101", "10010", "01101"],
    ["11110", "10001", "10001", "11110", "10100", "10010", "10001"],
    ["01111", "10000", "10000", "01110", "00001", "00001", "11110"],
    ["11111", "00100", "00100", "00100", "00100", "00100", "00100"],
    ["10001", "10001", "10001", "10001", "10001", "10001", "01110"],
    ["10001", "10001", "10001", "10001", "10001", "01010", "00100"],
    ["10001", "10001", "10001", "10101", "10101", "11011", "10001"],
    ["10001", "10001", "01010", "00100", "01010", "10001", "10001"],
    ["10001", "10001", "01010", "00100", "00100", "00100", "00100"],
    ["11111", "00001", "00010", "00100", "01000", "10000", "11111"],
] as const;
const DIGITS = ["01110", "10001", "10011", "10101", "11001", "10001", "01110"];

function glyph(character: string): readonly string[] {
    const direct = GLYPHS[character];
    if (direct) return direct;
    const upper = character.toUpperCase();
    const letterIndex = upper.charCodeAt(0) - 65;
    if (letterIndex >= 0 && letterIndex < 26) {
        const rows = LETTERS[letterIndex]!;
        return [...rows, "     "];
    }
    const digit = character.charCodeAt(0) - 48;
    if (digit >= 0 && digit <= 9) {
        if (digit === 0)
            return ["01110", "10001", "10011", "10101", "11001", "10001", "01110", "     "];
        const pattern = DIGITS;
        return [
            ...pattern.map((row, index) => (index === 0 ? (digit === 1 ? "00100" : row) : row)),
            "     ",
        ];
    }
    const seed = character.codePointAt(0) ?? 0;
    return Array.from({ length: 8 }, (_, row) =>
        Array.from({ length: 5 }, (_, column) =>
            (seed + row * 11 + column * 7) % 5 === 0 ? "█" : " ",
        ).join(""),
    );
}

function drawGlyph(
    pixels: Uint8Array,
    width: number,
    height: number,
    x: number,
    y: number,
    character: string,
    color: readonly [number, number, number],
): void {
    const rows = glyph(character);
    for (let row = 0; row < 8; row++) {
        const pattern = rows[row] ?? "     ";
        for (let column = 0; column < 5; column++) {
            if (pattern[column] !== "█" && pattern[column] !== "1") continue;
            const px = x + column;
            const py = y + row;
            if (px < 0 || py < 0 || px >= width || py >= height) continue;
            const offset = (py * width + px) * 3;
            pixels[offset] = color[0]!;
            pixels[offset + 1] = color[1]!;
            pixels[offset + 2] = color[2]!;
        }
    }
}

function drawText(
    pixels: Uint8Array,
    width: number,
    height: number,
    x: number,
    y: number,
    text: string,
    color: readonly [number, number, number],
): void {
    [...text].forEach((character, index) => {
        drawGlyph(pixels, width, height, x + index * MURAL_CELL_WIDTH, y, character, color);
    });
}

function fillRect(
    pixels: Uint8Array,
    canvasWidth: number,
    canvasHeight: number,
    x: number,
    y: number,
    width: number,
    height: number,
    color: readonly [number, number, number],
): void {
    const left = Math.max(0, x);
    const top = Math.max(0, y);
    const right = Math.min(canvasWidth, x + width);
    const bottom = Math.min(canvasHeight, y + height);
    for (let py = top; py < bottom; py++) {
        for (let px = left; px < right; px++) {
            const offset = (py * canvasWidth + px) * 3;
            pixels[offset] = color[0]!;
            pixels[offset + 1] = color[1]!;
            pixels[offset + 2] = color[2]!;
        }
    }
}

/** Round a content extent up to a complete vision tile without exceeding the cap. */
function snapDimensionToVisionTile(contentPixels: number, maximum: number): number {
    return Math.min(
        maximum,
        Math.max(
            MURAL_VISION_TILE,
            Math.ceil(contentPixels / MURAL_VISION_TILE) * MURAL_VISION_TILE,
        ),
    );
}

/**
 * Render the deterministic mural from a pre-ordered flat entry list. Zero LLM,
 * pure function of its input — callable any time. Category bands, bullet lines,
 * shared-pair packing, word-wrap at the column width, and prohibition ink are
 * all preserved from the author-era renderer; rooms and merges are gone.
 */
export function renderMural(entries: readonly MuralRenderEntry[]): MuralRenderResult {
    const layout = renderLayout(entries);
    const contentWidth =
        layout.columnCount === 0
            ? 0
            : layout.columnCount * MURAL_ROOM_WIDTH * MURAL_CELL_WIDTH +
              MURAL_COLUMN_GAP * (layout.columnCount - 1) * MURAL_CELL_WIDTH;
    const contentHeight = layout.rowCount * MURAL_LINE_PITCH;
    const width = snapDimensionToVisionTile(contentWidth, MURAL_WIDTH);
    const height = snapDimensionToVisionTile(contentHeight, MURAL_HEIGHT);
    const pixels = new Uint8Array(width * height * 3).fill(255);
    const left = Math.floor((width - contentWidth) / 2);
    for (let column = 0; column < layout.columnCount; column++) {
        for (let row = 0; row < layout.rowCount; row++) {
            const text = layout.grid[column]?.[row] ?? "";
            const isCategory = text.includes("<") && text.includes(">");
            const category = isCategory ? text.match(/<([^>]+)>/)?.[1] : undefined;
            if (isCategory)
                fillRect(
                    pixels,
                    width,
                    height,
                    left + column * (MURAL_ROOM_WIDTH + MURAL_COLUMN_GAP) * MURAL_CELL_WIDTH,
                    row * MURAL_LINE_PITCH,
                    MURAL_ROOM_WIDTH * MURAL_CELL_WIDTH,
                    MURAL_CELL_HEIGHT,
                    CATEGORY_COLORS[category ?? ""] ?? [72, 78, 86],
                );
            const ink = isCategory
                ? ([255, 255, 255] as const)
                : text.includes("⊘")
                  ? PROHIBITION_INK
                  : BODY_INK;
            drawText(
                pixels,
                width,
                height,
                left + column * (MURAL_ROOM_WIDTH + MURAL_COLUMN_GAP) * MURAL_CELL_WIDTH,
                row * MURAL_LINE_PITCH,
                text,
                ink,
            );
        }
    }
    const png = encodeRgbPng(pixels, width, height);
    const dataUrl = `data:image/png;base64,${Buffer.from(png).toString("base64")}`;
    return {
        png,
        dataUrl,
        muralText: layout.text,
        sha256Input: layout.text,
        placements: layout.placements,
        layoutItems: layout.layoutItems,
        renderedIds: layout.renderedIds,
        droppedIds: layout.droppedIds,
        categoryLineUsage: layout.usage,
        filledLineCount: layout.filledLineCount,
        width,
        height,
    };
}

export function muralImageTokenEstimateForDimensions(width: number, height: number): number {
    return Math.ceil((width * height) / 750);
}

export const muralImageTokenEstimate = muralImageTokenEstimateForDimensions(
    MURAL_WIDTH,
    MURAL_HEIGHT,
);
