import { deflateSync } from "node:zlib";

export const MURAL_WIDTH = 1_092;
export const MURAL_HEIGHT = 1_092;
export const MURAL_FONT = "spleen-5x8";
export const MURAL_CELL_WIDTH = 5;
export const MURAL_CELL_HEIGHT = 8;
export const MURAL_LINE_PITCH = 9;
export const MURAL_COLUMNS = 3;
export const MURAL_COLUMN_GAP = 1;
export const MURAL_ROOM_WIDTH = 72;
export const MURAL_ROWS = Math.floor(MURAL_HEIGHT / MURAL_LINE_PITCH);
export const MURAL_LINE_CAPACITY = MURAL_COLUMNS * MURAL_ROWS;

export const MURAL_CATEGORY_ORDER = [
    "PROJECT_RULES",
    "ARCHITECTURE",
    "CONSTRAINTS",
    "CONFIG_VALUES",
    "NAMING",
] as const;

export type MuralCategory = (typeof MURAL_CATEGORY_ORDER)[number] | string;

export interface MuralSpecEntry {
    id: number;
    category: MuralCategory;
    room: string;
    cue?: string | string[];
    mergeInto?: number;
    importance: number;
}

export interface MuralLayoutItem {
    kind: "category" | "room";
    category: MuralCategory;
    room?: string;
    column: number;
    startLine: number;
    endLine: number;
    continuation?: boolean;
    segment?: number;
}

export interface MuralRenderResult {
    png: Uint8Array;
    dataUrl: string;
    muralText: string;
    sha256Input: string;
    placements: Map<
        number,
        { category: MuralCategory; room: string; column: number; line: number; mergedInto?: number }
    >;
    layoutItems: MuralLayoutItem[];
    renderedIds: number[];
    droppedByTrimIds: number[];
    droppedBySkipIds: number[];
    categoryLineUsage: Record<string, number>;
}

type Cursor = { column: number; row: number };
type Room = { category: MuralCategory; name: string; entries: MuralSpecEntry[] };

const CATEGORY_COLORS: Record<string, readonly [number, number, number]> = {
    PROJECT_RULES: [24, 58, 112],
    ARCHITECTURE: [0, 88, 92],
    CONSTRAINTS: [126, 76, 16],
    CONFIG_VALUES: [88, 52, 132],
    NAMING: [28, 98, 58],
};
const BODY_INK: readonly [number, number, number] = [18, 20, 24];
const PROHIBITION_INK: readonly [number, number, number] = [148, 28, 35];
const ROOM_MARKER = "▰";

function codepoints(value: string): number {
    return [...value].length;
}

function categoryIndex(category: MuralCategory): number {
    const index = MURAL_CATEGORY_ORDER.indexOf(category as (typeof MURAL_CATEGORY_ORDER)[number]);
    return index >= 0 ? index : MURAL_CATEGORY_ORDER.length;
}

function cueText(entry: MuralSpecEntry): string {
    return Array.isArray(entry.cue) ? entry.cue.join("; ") : (entry.cue ?? "");
}

function escapeText(value: string): string {
    return value
        .replace(/[\r\n]+/g, " ")
        .replace(/\s+/g, " ")
        .trim();
}

function banner(category: MuralCategory): string {
    const label = ` <${category}> `;
    if (codepoints(label) > MURAL_ROOM_WIDTH)
        throw new Error(`category banner exceeds room width: ${category}`);
    const remaining = MURAL_ROOM_WIDTH - codepoints(label);
    return `${"─".repeat(Math.floor(remaining / 2))}${label}${"─".repeat(Math.ceil(remaining / 2))}`;
}

function wrapCue(cue: string): string[] {
    const words = cue.split(/\s+/).filter(Boolean);
    if (words.length === 0) throw new Error("mural cue is empty");
    const lines: string[] = [];
    let line = "•";
    for (const word of words) {
        if (codepoints(word) >= MURAL_ROOM_WIDTH)
            throw new Error(`mural cue anchor exceeds ${MURAL_ROOM_WIDTH} cells`);
        const candidate = line === "•" ? `${line}${word}` : `${line} ${word}`;
        if (codepoints(candidate) <= MURAL_ROOM_WIDTH) {
            line = candidate;
        } else {
            lines.push(line);
            line = ` ${word}`;
        }
    }
    lines.push(line);
    return lines;
}

function roomLines(
    room: Room,
    selected: MuralSpecEntry[],
): { lines: string[]; lineById: Map<number, number> } {
    const lines = [`${ROOM_MARKER} — ${room.name} —`];
    if (codepoints(lines[0]!) > MURAL_ROOM_WIDTH)
        throw new Error(`room header exceeds room width: ${room.name}`);
    const lineById = new Map<number, number>();
    for (const entry of selected) {
        if (entry.mergeInto !== undefined) continue;
        const body = wrapCue(escapeText(cueText(entry)));
        lineById.set(entry.id, lines.length);
        lines.push(...body);
    }
    return { lines, lineById };
}

function advance(cursor: Cursor): Cursor {
    if (cursor.row + 1 < MURAL_ROWS) return { column: cursor.column, row: cursor.row + 1 };
    return { column: cursor.column + 1, row: 0 };
}

function atEnd(cursor: Cursor): boolean {
    return cursor.column >= MURAL_COLUMNS;
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

function encodeRgbPng(pixels: Uint8Array): Uint8Array {
    const raw = new Uint8Array((MURAL_WIDTH * 3 + 1) * MURAL_HEIGHT);
    for (let y = 0; y < MURAL_HEIGHT; y++) {
        const rawStart = y * (MURAL_WIDTH * 3 + 1);
        raw[rawStart] = 0;
        raw.set(pixels.subarray(y * MURAL_WIDTH * 3, (y + 1) * MURAL_WIDTH * 3), rawStart + 1);
    }
    const ihdr = new Uint8Array(13);
    const header = new DataView(ihdr.buffer);
    header.setUint32(0, MURAL_WIDTH);
    header.setUint32(4, MURAL_HEIGHT);
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
            if (px < 0 || py < 0 || px >= MURAL_WIDTH || py >= MURAL_HEIGHT) continue;
            const offset = (py * MURAL_WIDTH + px) * 3;
            pixels[offset] = color[0]!;
            pixels[offset + 1] = color[1]!;
            pixels[offset + 2] = color[2]!;
        }
    }
}

function drawText(
    pixels: Uint8Array,
    x: number,
    y: number,
    text: string,
    color: readonly [number, number, number],
): void {
    [...text].forEach((character, index) => {
        drawGlyph(pixels, x + index * MURAL_CELL_WIDTH, y, character, color);
    });
}

function fillRect(
    pixels: Uint8Array,
    x: number,
    y: number,
    width: number,
    height: number,
    color: readonly [number, number, number],
): void {
    const left = Math.max(0, x);
    const top = Math.max(0, y);
    const right = Math.min(MURAL_WIDTH, x + width);
    const bottom = Math.min(MURAL_HEIGHT, y + height);
    for (let py = top; py < bottom; py++) {
        for (let px = left; px < right; px++) {
            const offset = (py * MURAL_WIDTH + px) * 3;
            pixels[offset] = color[0]!;
            pixels[offset + 1] = color[1]!;
            pixels[offset + 2] = color[2]!;
        }
    }
}

function allocateCategoryBudgets(rooms: Room[]): Map<MuralCategory, number> {
    const demand = new Map<MuralCategory, number>();
    for (const room of rooms)
        demand.set(room.category, (demand.get(room.category) ?? 0) + room.entries.length + 1);
    const categories = [...new Set(rooms.map((room) => room.category))].sort(
        (a, b) => categoryIndex(a) - categoryIndex(b),
    );
    const base = Math.floor(MURAL_LINE_CAPACITY / Math.max(1, MURAL_CATEGORY_ORDER.length));
    const budgets = new Map<MuralCategory, number>();
    let spare = MURAL_LINE_CAPACITY;
    for (const category of categories) {
        const amount = Math.min(base, demand.get(category) ?? 0);
        budgets.set(category, amount);
        spare -= amount;
    }
    while (spare > 0) {
        const unmet = categories.filter(
            (category) => (budgets.get(category) ?? 0) < (demand.get(category) ?? 0),
        );
        if (unmet.length === 0) break;
        for (const category of unmet) {
            if (spare <= 0) break;
            budgets.set(category, (budgets.get(category) ?? 0) + 1);
            spare--;
        }
    }
    return budgets;
}

function renderLayout(specs: MuralSpecEntry[]): {
    text: string;
    grid: string[][];
    placements: Map<
        number,
        MuralRenderResult["placements"] extends Map<number, infer V> ? V : never
    >;
    layoutItems: MuralLayoutItem[];
    renderedIds: number[];
    droppedByTrimIds: number[];
    droppedBySkipIds: number[];
    usage: Record<string, number>;
} {
    const grouped = new Map<string, Room>();
    for (const spec of specs) {
        const key = `${spec.category}\u0000${spec.room}`;
        const room = grouped.get(key) ?? { category: spec.category, name: spec.room, entries: [] };
        room.entries.push(spec);
        grouped.set(key, room);
    }
    const rooms = [...grouped.values()].sort(
        (a, b) => categoryIndex(a.category) - categoryIndex(b.category),
    );
    const budgets = allocateCategoryBudgets(rooms);
    const grid = Array.from({ length: MURAL_COLUMNS }, () =>
        Array.from({ length: MURAL_ROWS }, () => ""),
    );
    const placements = new Map<
        number,
        { category: MuralCategory; room: string; column: number; line: number; mergedInto?: number }
    >();
    const layoutItems: MuralLayoutItem[] = [];
    const renderedIds: number[] = [];
    const droppedByTrimIds: number[] = [];
    const droppedBySkipIds: number[] = [];
    const usage: Record<string, number> = {};
    let cursor: Cursor = { column: 0, row: 0 };
    for (const category of [...new Set(rooms.map((room) => room.category))].sort(
        (a, b) => categoryIndex(a) - categoryIndex(b),
    )) {
        const categoryRooms = rooms.filter((room) => room.category === category);
        let remaining = budgets.get(category) ?? 0;
        let categoryRendered = false;
        for (const room of categoryRooms) {
            const includeCategoryBanner = !categoryRendered;
            const includeGap = categoryRendered;
            const original = room.entries;
            let selected: MuralSpecEntry[] = [];
            let plan: { lines: string[]; lineById: Map<number, number> } | null = null;
            for (let keep = original.length; keep >= 1; keep--) {
                const candidate = original.slice(0, keep);
                if (
                    candidate.some(
                        (entry) =>
                            entry.mergeInto !== undefined &&
                            !candidate.some(
                                (target) =>
                                    target.id === entry.mergeInto && target.mergeInto === undefined,
                            ),
                    )
                )
                    continue;
                const nextPlan = roomLines(
                    room,
                    candidate.filter((entry) => entry.mergeInto === undefined),
                );
                const fixed = (includeCategoryBanner ? 1 : 0) + (includeGap ? 1 : 0);
                if (nextPlan.lines.length + fixed <= remaining) {
                    selected = candidate;
                    plan = nextPlan;
                    break;
                }
            }
            if (!plan) {
                droppedBySkipIds.push(...original.map((entry) => entry.id));
                continue;
            }
            const dropped = original.slice(selected.length).map((entry) => entry.id);
            droppedByTrimIds.push(...dropped);
            if (includeCategoryBanner) {
                if (cursor.row >= MURAL_ROWS - 1) cursor = advance(cursor);
                if (atEnd(cursor)) break;
                grid[cursor.column]![cursor.row] = banner(category);
                layoutItems.push({
                    kind: "category",
                    category,
                    column: cursor.column,
                    startLine: cursor.row + 1,
                    endLine: cursor.row + 1,
                });
                cursor = advance(cursor);
            }
            if (includeGap && !atEnd(cursor)) {
                if (cursor.row >= MURAL_ROWS - 2) cursor = advance(cursor);
                if (!atEnd(cursor)) {
                    grid[cursor.column]![cursor.row] = "";
                    cursor = advance(cursor);
                }
            }
            if (atEnd(cursor)) break;
            categoryRendered = true;
            const startColumn = cursor.column;
            const startRow = cursor.row;
            const segmentStart = cursor;
            let segment = 0;
            let segmentStartCursor = segmentStart;
            const lineSlots: Cursor[] = [];
            for (const line of plan.lines) {
                if (atEnd(cursor)) break;
                if (cursor.row === MURAL_ROWS - 1 && line.startsWith(ROOM_MARKER))
                    cursor = advance(cursor);
                if (atEnd(cursor)) break;
                grid[cursor.column]![cursor.row] = line;
                lineSlots.push(cursor);
                cursor = advance(cursor);
                if (cursor.column !== segmentStartCursor.column && lineSlots.length > 0) {
                    layoutItems.push({
                        kind: "room",
                        category,
                        room: room.name,
                        column: segmentStartCursor.column,
                        startLine: segmentStartCursor.row + 1,
                        endLine: MURAL_ROWS - 1 + 1,
                        continuation: segment > 0,
                        segment,
                    });
                    segment++;
                    segmentStartCursor = cursor;
                }
            }
            if (lineSlots.length === 0) {
                droppedBySkipIds.push(...selected.map((entry) => entry.id));
                continue;
            }
            const last = lineSlots.at(-1)!;
            if (segmentStartCursor.column === last.column)
                layoutItems.push({
                    kind: "room",
                    category,
                    room: room.name,
                    column: segmentStartCursor.column,
                    startLine: segmentStartCursor.row + 1,
                    endLine: last.row + 1,
                    continuation: segment > 0,
                    segment,
                });
            for (const entry of selected) {
                const bodyLine =
                    entry.mergeInto === undefined
                        ? plan.lineById.get(entry.id)
                        : plan.lineById.get(entry.mergeInto);
                if (bodyLine === undefined || !lineSlots[bodyLine]) continue;
                const slot = lineSlots[bodyLine];
                placements.set(entry.id, {
                    category,
                    room: room.name,
                    column: slot.column,
                    line: slot.row + 1,
                    ...(entry.mergeInto === undefined ? {} : { mergedInto: entry.mergeInto }),
                });
                renderedIds.push(entry.id);
            }
            const consumed =
                (includeCategoryBanner ? 1 : 0) + (includeGap ? 1 : 0) + plan.lines.length;
            remaining = Math.max(0, remaining - consumed);
            usage[category] = (usage[category] ?? 0) + consumed;
            void startColumn;
            void startRow;
        }
    }
    const lines = Array.from({ length: MURAL_ROWS }, (_, row) =>
        Array.from({ length: MURAL_COLUMNS }, (_, column) =>
            (grid[column]?.[row] ?? "").padEnd(MURAL_ROOM_WIDTH),
        ).join(" "),
    );
    return {
        text: `${lines.join("\n")}\n`,
        grid,
        placements,
        layoutItems,
        renderedIds,
        droppedByTrimIds,
        droppedBySkipIds,
        usage,
    };
}

export function renderMural(specs: MuralSpecEntry[]): MuralRenderResult {
    if (specs.length === 0) throw new Error("cannot render an empty mural");
    const layout = renderLayout(specs);
    const pixels = new Uint8Array(MURAL_WIDTH * MURAL_HEIGHT * 3).fill(255);
    const contentWidth = MURAL_COLUMNS * MURAL_ROOM_WIDTH + MURAL_COLUMN_GAP * (MURAL_COLUMNS - 1);
    const left = Math.floor((MURAL_WIDTH - contentWidth * MURAL_CELL_WIDTH) / 2);
    for (let column = 0; column < MURAL_COLUMNS; column++) {
        for (let row = 0; row < MURAL_ROWS; row++) {
            const text = layout.grid[column]?.[row] ?? "";
            const isCategory = text.includes("<") && text.includes(">");
            const category = isCategory ? text.match(/<([^>]+)>/)?.[1] : undefined;
            if (isCategory)
                fillRect(
                    pixels,
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
                left + column * (MURAL_ROOM_WIDTH + MURAL_COLUMN_GAP) * MURAL_CELL_WIDTH,
                row * MURAL_LINE_PITCH,
                text,
                ink,
            );
        }
    }
    const png = encodeRgbPng(pixels);
    const dataUrl = `data:image/png;base64,${Buffer.from(png).toString("base64")}`;
    return {
        png,
        dataUrl,
        muralText: layout.text,
        sha256Input: layout.text,
        placements: layout.placements,
        layoutItems: layout.layoutItems,
        renderedIds: layout.renderedIds,
        droppedByTrimIds: layout.droppedByTrimIds,
        droppedBySkipIds: layout.droppedBySkipIds,
        categoryLineUsage: layout.usage,
    };
}

export const muralImageTokenEstimate = Math.ceil((MURAL_WIDTH * MURAL_HEIGHT) / 750);
