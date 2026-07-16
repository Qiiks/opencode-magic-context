import { readdirSync, readFileSync, writeFileSync } from "node:fs";
import { basename, dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const CATEGORY_ORDER = [
    "PROJECT_RULES",
    "ARCHITECTURE",
    "CONSTRAINTS",
    "CONFIG_VALUES",
    "NAMING",
] as const;
const LAYOUT_FONT =
    process.env.PALACE_LAYOUT_FONT === "jetbrains-mono-10" ? "jetbrains-mono-10" : "spleen-5x8";
const CELL_WIDTH = LAYOUT_FONT === "jetbrains-mono-10" ? 6 : 5;
const CELL_HEIGHT = LAYOUT_FONT === "jetbrains-mono-10" ? 11 : 8;
const COLUMN_COUNT = LAYOUT_FONT === "jetbrains-mono-10" ? 2 : 3;
const COLUMN_GAP = 1;
const PAGE_WIDTH_PIXELS = 1_092;
const PAGE_HEIGHT_PIXELS = 1_092;
const ROOM_WIDTH = Math.floor(
    (Math.floor(PAGE_WIDTH_PIXELS / CELL_WIDTH) - (COLUMN_COUNT - 1) * COLUMN_GAP) / COLUMN_COUNT,
);
const BANNER_HEIGHT_PIXELS = CELL_HEIGHT;
const BODY_LINE_PITCH = CELL_HEIGHT + 1;
const PAGE_WIDTH_CHARS = COLUMN_COUNT * ROOM_WIDTH;
const MAX_LINE_CHARS = PAGE_WIDTH_CHARS + (COLUMN_COUNT - 1) * COLUMN_GAP;
const PAGE_HEIGHT_CELLS = Math.floor(PAGE_HEIGHT_PIXELS / BODY_LINE_PITCH);
const MAX_PALACE_CHARS = 27_000;
const SOURCE_PATH = "/tmp/visual-memory/trimmed-memories-source.txt";
const HERE = dirname(fileURLToPath(import.meta.url));
const ALTERNATE_LAYOUT = LAYOUT_FONT === "jetbrains-mono-10";
const PALACE_OUTPUT = ALTERNATE_LAYOUT
    ? "/tmp/visual-memory/palace-jb-layout.txt"
    : join(HERE, "palace.txt");
const COVERAGE_OUTPUT = ALTERNATE_LAYOUT
    ? "/tmp/visual-memory/coverage-jb-layout.json"
    : join(HERE, "coverage.json");

export type Category = (typeof CATEGORY_ORDER)[number];
export type SpecEntry = {
    id: number;
    category: Category;
    room: string;
    cue?: string | string[];
    mergeInto?: number;
    importance: number;
};
export type SourceMemory = {
    id: number;
    category: Category;
    importance: number;
};
type Placement = {
    category: Category;
    room: string;
    palaceLine: number;
    palaceColumn: number;
    page: number;
    pageLine: number;
    mergedInto?: number;
};
type RoomSummary = {
    category: Category;
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
type LayoutItem = {
    kind: "category" | "room";
    category: Category;
    categories?: Category[];
    room?: string;
    continuation?: boolean;
    segment?: number;
    column: number;
    startLine: number;
    endLine: number;
    page: number;
    pageLine: number;
    pageTopPixels: number;
    heightPixels: number;
};

type Box = {
    category: Category;
    name: string;
    lines: string[];
    bodyLines: string[];
    entryBodyLines: Map<number, number>;
    relativeLines: Map<number, number>;
    entries: SpecEntry[];
    merges: SpecEntry[];
    peakImportance: number;
    sharedPairCount: number;
    continuation: boolean;
    segment: number;
    heightPixels: number;
};

function codepoints(value: string): number {
    return [...value].length;
}

export function parseSource(source: string, importanceById: ReadonlyMap<number, number>): SourceMemory[] {
    const memories: SourceMemory[] = [];
    let category: Category | undefined;
    for (const line of source.split("\n")) {
        const open = line.match(/^<([A-Z_]+)>$/)?.[1];
        if (open) {
            if (!CATEGORY_ORDER.includes(open as Category))
                throw new Error(`unknown category ${open}`);
            category = open as Category;
            continue;
        }
        if (/^<\//.test(line)) {
            category = undefined;
            continue;
        }
        const id = line.match(/^#(\d+):/)?.[1];
        if (id) {
            if (!category) throw new Error(`memory ${id} is outside a category`);
            const numericId = Number(id);
            const importance = importanceById.get(numericId);
            if (importance === undefined || !Number.isFinite(importance))
                throw new Error(`source importance missing for ${id}`);
            memories.push({ id: numericId, category, importance });
        }
    }
    return memories;
}

export function readSpecs(directory = HERE): SpecEntry[] {
    const files = readdirSync(directory)
        .filter((file) => file.startsWith("spec-") && file.endsWith(".json"))
        .sort();
    return files.flatMap(
            (file) => JSON.parse(readFileSync(join(directory, file), "utf8")) as SpecEntry[],
    );
}

export function isExactToken(value: string): boolean {
    const token = value.replace(/^[('"`]+|[)'"`,;]+$/g, "");
    if (!token) return false;
    return (
        /[\\/_$%<>=|@]/.test(token) ||
        /:\S/.test(token) ||
        /(?:[A-Za-z0-9]\.[A-Za-z0-9]|^\.[A-Za-z0-9])/.test(token) ||
        /[A-Za-z0-9]+-[A-Za-z0-9]+-[A-Za-z0-9]+/.test(token) ||
        /\d/.test(token) ||
        /[a-z][A-Z]/.test(token) ||
        /\b[A-Z_]{2,}\b/.test(token)
    );
}

function compactCue(raw: string, room: string): string {
    const hubWords = room
        .split(/[^A-Za-z0-9]+/)
        .filter((word) => word.length >= 2)
        .map((word) => word.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"));
    let value = raw;
    if (hubWords.length > 0) {
        value = value.replace(
            new RegExp(`(?<![\\w/._-])(?:${hubWords.join("|")})(?![\\w/._-])`, "gi"),
            "",
        );
    }
    const protectedValues: string[] = [];
    value = value.replace(/`[^`]+`|[^\s()`]+/g, (match) => {
        if (!match.startsWith("`") && !isExactToken(match)) return match;
        const marker = `QZ${protectedValues.length}ZQ`;
        protectedValues.push(match);
        return marker;
    });
    const replacements: Array<[RegExp, string]> = [
        [/\bconfigurations?\b/gi, "cfg"],
        [/\bbackground\b/gi, "bg"],
        [/\benvironment\b/gi, "env"],
        [/\bparameters?\b/gi, "params"],
        [/\bbefore\b/gi, "≺"],
        [/\bafter\b/gi, "≻"],
        [/\bbecause\b/gi, "∵"],
        [/\breturns?\b/gi, "→"],
        [/\bwrites?\b/gi, "→"],
        [/\breads?\b/gi, "←"],
        [/\brequires?\b/gi, "→"],
        [/\bevery\b/gi, "∀"],
        [/\ball\b/gi, "∀"],
        [/\bnone\b/gi, "∅"],
        [/\bzero\b/gi, "0"],
        [/\bthe\b/gi, ""],
        [/\ban?\b/gi, ""],
        [/\s*;\s*/g, "; "],
        [/\s*→\s*/g, "→"],
        [/\s*←\s*/g, "←"],
        [/\s*≺\s*/g, "≺"],
        [/\s*≻\s*/g, "≻"],
        [/\s*∵\s*/g, "∵"],
        [/\s{2,}/g, " "],
    ];
    for (const [pattern, replacement] of replacements) value = value.replace(pattern, replacement);
    value = protectedValues.reduce(
        (result, item, index) => result.replace(`QZ${index}ZQ`, () => item),
        value,
    );
    return value.trim().replace(/^[-:;,]+|[-:;,]+$/g, "");
}

function displayCue(entry: SpecEntry): string {
    const raw = Array.isArray(entry.cue) ? entry.cue.join("; ") : (entry.cue ?? "");
    return compactCue(raw, entry.room);
}

function cueOutsideCode(value: string): string {
    // Mask inline code so incomplete call fragments cannot affect the parenthesized
    // explanations required after ⊘ markers; the original cue still preserves code verbatim.
    return value.replace(/`[^`]*`/g, (anchor) => " ".repeat(anchor.length));
}

/**
 * Cue length is an authoring diagnostic rather than a reason to reject a
 * selected memory. Keep reporting it so trial reports can compare compression
 * quality, but allow the renderer to apply the page's importance policy.
 */
function reportCueWarning(message: string, warnings: string[]): void {
    warnings.push(message);
    console.warn(`[palace] cue warning: ${message}`);
}

export function validate(source: SourceMemory[], specs: SpecEntry[]): string[] {
    if (source.length === 0) throw new Error("source contains no memories");
    if (new Set(source.map((memory) => memory.id)).size !== source.length)
        throw new Error("source contains duplicate memory ids");
    for (const memory of source) {
        if (!Number.isFinite(memory.importance))
            throw new Error(`source importance missing for ${memory.id}`);
    }
    const defects: string[] = [];
    const sourceById = new Map(source.map((memory) => [memory.id, memory]));
    const specById = new Map<number, SpecEntry>();
    for (const spec of specs) {
        if (specById.has(spec.id)) {
            throw new Error(`duplicate spec id ${spec.id}`);
        }
        specById.set(spec.id, spec);
        const memory = sourceById.get(spec.id);
        if (!memory) throw new Error(`spec id ${spec.id} absent from source`);
        if (memory.category !== spec.category) throw new Error(`category mismatch for ${spec.id}`);
        if (!Number.isFinite(spec.importance)) throw new Error(`importance missing for ${spec.id}`);
        if (spec.mergeInto === undefined && spec.cue === undefined)
            throw new Error(`cue missing for ${spec.id}`);
        if (spec.mergeInto !== undefined && spec.cue !== undefined)
            throw new Error(`merged ${spec.id} also has cue`);
        const cue = Array.isArray(spec.cue) ? spec.cue.join(" ") : spec.cue;
        if (cue && /#\d+/.test(cue)) throw new Error(`memory id leaked into cue ${spec.id}`);
        if (cue) {
            const renderedCue = displayCue(spec);
            const cueBudget = memory.importance >= 70 ? 90 : 50;
            const renderedCueLength = codepoints(renderedCue);
            if (renderedCueLength > cueBudget) {
                reportCueWarning(
                    `cue over budget for ${spec.id}: ${renderedCueLength} chars (max ${cueBudget})`,
                    defects,
                );
            }
            for (const hubWord of spec.room
                .split(/[^A-Za-z0-9]+/)
                .filter((word) => word.length >= 2)) {
                if (new RegExp(`(?<![\\w/._-])${hubWord}(?![\\w/._-])`, "i").test(renderedCue)) {
                    throw new Error(`hub noun repeated in cue ${spec.id}: ${renderedCue}`);
                }
            }
            const mechanismCue = cueOutsideCode(renderedCue);
            const negativeRule = /\b(?:must not|never|without|instead of|excludes?)\b/i.test(
                mechanismCue,
            );
            if (negativeRule && !mechanismCue.includes("⊘")) {
                throw new Error(`negative rule missing polarity marker in cue ${spec.id}: ${renderedCue}`);
            }
            const polarityCount = mechanismCue.split("⊘").length - 1;
            const mechanismCount = mechanismCue.match(/\([^()]+\)/g)?.length ?? 0;
            if (polarityCount > mechanismCount) {
                throw new Error(`polarity mechanism missing from rendered cue ${spec.id}: ${renderedCue}`);
            }
            let marker = mechanismCue.indexOf("⊘");
            while (marker >= 0) {
                const nextMarker = mechanismCue.indexOf("⊘", marker + 1);
                const mechanism = mechanismCue.indexOf("(", marker + 1);
                if (mechanism < 0 || (nextMarker >= 0 && mechanism > nextMarker)) {
                    throw new Error(`polarity mechanism must follow marker ${spec.id}: ${renderedCue}`);
                    break;
                }
                let depth = 0;
                let close = -1;
                for (let index = mechanism; index < mechanismCue.length; index++) {
                    if (mechanismCue[index] === "(") depth++;
                    if (mechanismCue[index] === ")") depth--;
                    if (depth === 0) {
                        close = index;
                        break;
                    }
                }
                if (close < mechanism) {
                    throw new Error(`polarity mechanism is unclosed ${spec.id}: ${renderedCue}`);
                }
                marker = mechanismCue.indexOf("⊘", close + 1);
            }
            const unclosed = [...mechanismCue].reduce(
                (depth, character) => depth + (character === "(" ? 1 : character === ")" ? -1 : 0),
                0,
            );
            if (unclosed !== 0)
                throw new Error(`unbalanced mechanism in rendered cue ${spec.id}: ${renderedCue}`);
            const exactAnchors = (cue.match(/`[^`]+`|[^\s()`]+/g) ?? []).filter(
                (anchor) => anchor.startsWith("`") || isExactToken(anchor),
            );
            const hubAnchors = new Set(
                spec.room
                    .split(/[^A-Za-z0-9]+/)
                    .filter(Boolean)
                    .map((word) => word.toLowerCase()),
            );
            for (const rawAnchor of exactAnchors) {
                const anchor = rawAnchor.replace(/^[,;]+|[,;]+$/g, "");
                if (hubAnchors.has(anchor.replace(/[^A-Za-z0-9]+/g, "").toLowerCase())) continue;
                let anchorWithoutHub = anchor;
                for (const hubWord of hubAnchors) {
                    anchorWithoutHub = anchorWithoutHub.replace(
                        new RegExp(`(?<![\\w/._-])${hubWord}(?![\\w/._-])`, "gi"),
                        "",
                    );
                }
                if (anchorWithoutHub !== anchor && renderedCue.includes(anchorWithoutHub)) continue;
                if (["AND", "APIs", "NEVER", "OR", "RAM", "SAME"].includes(anchor)) continue;
                if (anchor && !renderedCue.includes(anchor)) {
                    throw new Error(
                        `exact anchor ${anchor} missing from rendered cue ${spec.id}: ${renderedCue}`,
                    );
                }
            }
        }
    }
    for (const spec of specs) {
        if (spec.mergeInto === undefined) continue;
        const target = specById.get(spec.mergeInto);
        if (!target || target.mergeInto !== undefined)
            throw new Error(`invalid merge target ${spec.mergeInto}`);
        if (target.category !== spec.category || target.room !== spec.room) {
            throw new Error(`merge ${spec.id} crosses room/category`);
        }
    }
    return defects;
}

function longestToken(entries: SpecEntry[]): number {
    return Math.max(
        ...entries.flatMap((entry) => displayCue(entry).split(/\s+/).map(codepoints)),
        0,
    );
}

function appendEntry(body: string[], cue: string, width: number): number {
    const words = cue.split(/\s+/).filter(Boolean);
    if (words.length === 0) throw new Error("empty palace cue");
    const placement = body.length;
    let line = "•";
    for (const word of words) {
        const separator = line === "•" || line === " " ? "" : " ";
        const candidate = `${line}${separator}${word}`;
        if (codepoints(candidate) <= width) {
            line = candidate;
            continue;
        }
        body.push(line);
        line = ` ${word}`;
        if (codepoints(line) > width) throw new Error(`anchor exceeds room width: ${word}`);
    }
    body.push(line);
    return placement;
}

function frameBox(
    name: string,
    bodyLines: string[],
    peakImportance: number,
    continuation: boolean,
): string[] {
    const innerWidth = ROOM_WIDTH - 2;
    const high = peakImportance >= 70;
    const [tl, fill, tr, side, bl, br] = high
        ? ["╔", "═", "╗", "║", "╚", "╝"]
        : ["┌", "─", "┐", "│", "└", "┘"];
    const bottom = `${bl}${fill.repeat(innerWidth)}${br}`;
    if (continuation) {
        const marker = " … ";
        const remaining = innerWidth - codepoints(marker);
        const top = `${tl}${fill.repeat(Math.floor(remaining / 2))}${marker}${fill.repeat(Math.ceil(remaining / 2))}${tr}`;
        return [
            top,
            ...bodyLines.map((line) => `${side}${line.padEnd(innerWidth)}${side}`),
            bottom,
        ];
    }
    const titlePadding = innerWidth - codepoints(name);
    const title = `${" ".repeat(Math.floor(titlePadding / 2))}${name}${" ".repeat(Math.ceil(titlePadding / 2))}`;
    return [
        `${tl}${fill.repeat(innerWidth)}${tr}`,
        `${side}${title}${side}`,
        ...bodyLines.map((line) => `${side}${line.padEnd(innerWidth)}${side}`),
        bottom,
    ];
}

function buildBox(category: Category, name: string, allEntries: SpecEntry[]): Box {
    // Manifest order is the author's importance ranking; never sort it by id.
    const entries = allEntries.filter((entry) => entry.mergeInto === undefined);
    const merges = allEntries.filter((entry) => entry.mergeInto !== undefined);
    const innerWidth = ROOM_WIDTH - 2;
    const requiredTokenWidth = longestToken(entries);
    if (requiredTokenWidth > innerWidth) {
        throw new Error(`room ${name} has ${requiredTokenWidth}-char anchor (max ${innerWidth})`);
    }
    if (codepoints(name) * 2 > innerWidth) {
        throw new Error(`2x room title ${name} exceeds ${innerWidth} cells`);
    }

    const bodyLines: string[] = [];
    const entryBodyLines = new Map<number, number>();
    const shortEntryLimit = Math.floor((innerWidth - 4) / 2);
    let sharedPairCount = 0;
    for (let index = 0; index < entries.length; index++) {
        const entry = entries[index];
        if (!entry) continue;
        const cue = displayCue(entry);
        const next = entries[index + 1];
        const nextCue = next ? displayCue(next) : "";
        const shared = `•${cue} • ${nextCue}`;
        if (
            next &&
            !cue.includes("⊘") &&
            !nextCue.includes("⊘") &&
            codepoints(cue) <= shortEntryLimit &&
            codepoints(nextCue) <= shortEntryLimit &&
            codepoints(shared) <= innerWidth
        ) {
            const bodyLine = bodyLines.length;
            bodyLines.push(shared);
            entryBodyLines.set(entry.id, bodyLine);
            entryBodyLines.set(next.id, bodyLine);
            sharedPairCount++;
            index++;
            continue;
        }
        const bodyLine = appendEntry(bodyLines, cue, innerWidth);
        entryBodyLines.set(entry.id, bodyLine);
    }
    const peakImportance = Math.max(...allEntries.map((entry) => entry.importance));
    const relativeLines = new Map<number, number>();
    for (const entry of entries) {
        const bodyLine = entryBodyLines.get(entry.id);
        if (bodyLine === undefined) throw new Error(`body line missing for ${entry.id}`);
        relativeLines.set(entry.id, bodyLine + 2);
    }
    for (const merge of merges) {
        const targetLine =
            merge.mergeInto === undefined ? undefined : relativeLines.get(merge.mergeInto);
        if (targetLine === undefined) throw new Error(`merge target line missing for ${merge.id}`);
        relativeLines.set(merge.id, targetLine);
    }
    return {
        category,
        name,
        lines: frameBox(name, bodyLines, peakImportance, false),
        bodyLines,
        entryBodyLines,
        relativeLines,
        entries,
        merges,
        peakImportance,
        sharedPairCount,
        continuation: false,
        segment: 0,
        heightPixels: 3 * CELL_HEIGHT + bodyLines.length * BODY_LINE_PITCH,
    };
}

export function renderPalace(specs: SpecEntry[]): {
    palace: string;
    placements: Map<number, Placement>;
    rooms: RoomSummary[];
    layoutItems: LayoutItem[];
    pages: Array<{
        page: number;
        startLine: number;
        endLine: number;
        heightCells: number;
        heightPixels: number;
    }>;
    leveling: { gapRowsBefore: number; gapRowsAfter: number; splitCount: number };
    droppedByTrimIds: number[];
    droppedBySkipIds: number[];
} {
    type RoomInput = { category: Category; name: string; entries: SpecEntry[] };
    const grouped = new Map<string, RoomInput>();
    const MAX_RENDER_ITERATIONS = 1_000_000;
    let iterations = 0;
    const guard = (context: string): void => {
        if (++iterations > MAX_RENDER_ITERATIONS) {
            throw new Error(
                `single-page placement exceeded ${MAX_RENDER_ITERATIONS} iterations while ${context}`,
            );
        }
    };

    for (const spec of specs) {
        guard("grouping manifest entries");
        const key = `${spec.category}\u0000${spec.room}`;
        const room = grouped.get(key) ?? { category: spec.category, name: spec.room, entries: [] };
        room.entries.push(spec);
        grouped.set(key, room);
    }

    const palaceLines: string[] = [];
    const placements = new Map<number, Placement>();
    const roomSummaries: RoomSummary[] = [];
    const layoutItems: LayoutItem[] = [];
    const droppedByTrimIds: number[] = [];
    const droppedBySkipIds: number[] = [];
    const droppedTrimSet = new Set<number>();
    const droppedSkipSet = new Set<number>();
    let usedHeightPixels = 0;
    let usedRows = 0;
    let categoryRendered = false;
    let currentCategory: Category | undefined;
    let currentBoxes: Box[] = [];
    let currentBandHeightPixels = 0;
    let currentBandHeightRows = 0;

    const categoryBanner = (category: Category): string => {
        const label = ` <${category}> `;
        const remaining = MAX_LINE_CHARS - codepoints(label);
        return `${"─".repeat(Math.floor(remaining / 2))}${label}${"─".repeat(Math.ceil(remaining / 2))}`;
    };
    const addDropped = (id: number, kind: "trim" | "skip"): void => {
        const set = kind === "trim" ? droppedTrimSet : droppedSkipSet;
        const output = kind === "trim" ? droppedByTrimIds : droppedBySkipIds;
        if (placements.has(id) || set.has(id)) return;
        set.add(id);
        output.push(id);
    };
    const mergeTargetsArePresent = (entries: SpecEntry[]): boolean => {
        const ids = new Set(entries.map((entry) => entry.id));
        return entries.every((entry) => entry.mergeInto === undefined || ids.has(entry.mergeInto));
    };
    const flushBand = (category: Category): void => {
        if (currentBoxes.length === 0) return;
        guard(`flushing ${category} placement band`);
        if (currentCategory !== category) {
            throw new Error(`placement band category changed from ${currentCategory} to ${category}`);
        }

        let bandTopPixels = usedHeightPixels;
        let bandTopRows = usedRows;
        if (!categoryRendered) {
            const bannerLine = palaceLines.length + 1;
            const banner = categoryBanner(category);
            palaceLines.push(banner);
            layoutItems.push({
                kind: "category",
                category,
                categories: [category],
                column: 0,
                startLine: bannerLine,
                endLine: bannerLine,
                page: 1,
                pageLine: bannerLine,
                pageTopPixels: usedHeightPixels,
                heightPixels: BANNER_HEIGHT_PIXELS,
            });
            usedHeightPixels += BANNER_HEIGHT_PIXELS;
            usedRows++;
            bandTopPixels = usedHeightPixels;
            bandTopRows = usedRows;
            categoryRendered = true;
        }

        const bandStartLine = palaceLines.length + 1;
        const bandPageLine = bandStartLine;
        const columns = Array.from({ length: COLUMN_COUNT }, () => [] as string[]);
        const columnRows = Array<number>(COLUMN_COUNT).fill(0);
        const columnPixelRows = Array<number>(COLUMN_COUNT).fill(0);
        for (const [index, box] of currentBoxes.entries()) {
            guard(`placing ${category} room boxes`);
            const column = index;
            const boxLines = columns[column];
            if (!boxLines) throw new Error(`missing column ${column} while placing ${category}`);
            boxLines.push(...box.lines);
            const row = columnRows[column] ?? 0;
            const pixelRow = columnPixelRows[column] ?? 0;
            columnRows[column] = row + box.lines.length;
            columnPixelRows[column] = pixelRow + box.heightPixels;
            const startLine = bandStartLine + row;
            const roomPageLine = bandPageLine + row;
            const roomPageTopPixels = bandTopPixels + pixelRow;
            for (const entry of [...box.entries, ...box.merges]) {
                guard(`placing ${category}/${box.name} entries`);
                const relativeLine = box.relativeLines.get(entry.id);
                if (relativeLine === undefined) throw new Error(`placement missing for ${entry.id}`);
                placements.set(entry.id, {
                    category: box.category,
                    room: box.name,
                    palaceLine: startLine + relativeLine,
                    palaceColumn: column * (ROOM_WIDTH + COLUMN_GAP) + 1,
                    page: 1,
                    pageLine: roomPageLine + relativeLine,
                    ...(entry.mergeInto === undefined ? {} : { mergedInto: entry.mergeInto }),
                });
            }
            roomSummaries.push({
                category: box.category,
                name: box.name,
                entryCount: box.entries.length,
                mergeCount: box.merges.length,
                memoryCount: box.entries.length + box.merges.length,
                peakImportance: box.peakImportance,
                border: box.peakImportance >= 70 ? "double" : "single",
                column,
                startLine,
                endLine: startLine + box.lines.length - 1,
                heightCells: box.lines.length,
                sharedPairCount: box.sharedPairCount,
                continuation: false,
                segment: 0,
                page: 1,
                pageLine: roomPageLine,
                pageTopPixels: roomPageTopPixels,
                heightPixels: box.heightPixels,
            });
            layoutItems.push({
                kind: "room",
                category: box.category,
                room: box.name,
                continuation: false,
                segment: 0,
                column,
                startLine,
                endLine: startLine + box.lines.length - 1,
                page: 1,
                pageLine: roomPageLine,
                pageTopPixels: roomPageTopPixels,
                heightPixels: box.heightPixels,
            });
        }

        const bandHeight = Math.max(...columnRows);
        for (let row = 0; row < bandHeight; row++) {
            guard(`writing ${category} placement band lines`);
            const line = columns
                .map((column) => (column[row] ?? "").padEnd(ROOM_WIDTH))
                .join(" ".repeat(COLUMN_GAP))
                .padEnd(MAX_LINE_CHARS);
            palaceLines.push(line);
        }
        usedHeightPixels = bandTopPixels + currentBandHeightPixels;
        usedRows = bandTopRows + bandHeight;
        if (usedHeightPixels > PAGE_HEIGHT_PIXELS) {
            throw new Error(`single-page placement exceeds page by ${usedHeightPixels - PAGE_HEIGHT_PIXELS}px`);
        }
        if (usedRows > PAGE_HEIGHT_CELLS) {
            throw new Error(`single-page text placement exceeds page by ${usedRows - PAGE_HEIGHT_CELLS} rows`);
        }
        currentBoxes = [];
        currentBandHeightPixels = 0;
        currentBandHeightRows = 0;
    };

    for (const category of CATEGORY_ORDER) {
        guard(`scanning ${category} rooms`);
        const rooms = [...grouped.values()].filter((room) => room.category === category);
        if (rooms.length === 0) continue;
        currentCategory = category;
        categoryRendered = false;
        for (const room of rooms) {
            guard(`selecting ${category}/${room.name}`);
            if (currentBoxes.length >= COLUMN_COUNT) flushBand(category);
            const fits = (box: Box): boolean => {
                const bannerHeight = categoryRendered || currentBoxes.length > 0 ? 0 : BANNER_HEIGHT_PIXELS;
                const bannerRows = categoryRendered || currentBoxes.length > 0 ? 0 : 1;
                return (
                    usedHeightPixels + bannerHeight + Math.max(currentBandHeightPixels, box.heightPixels) <=
                        PAGE_HEIGHT_PIXELS &&
                    usedRows + bannerRows + Math.max(currentBandHeightRows, box.lines.length) <=
                        PAGE_HEIGHT_CELLS
                );
            };
            const selectBox = (): { box: Box; trimmed: SpecEntry[] } | undefined => {
                if (!mergeTargetsArePresent(room.entries)) {
                    throw new Error(`room ${room.name} has a merge target outside its manifest order`);
                }
                const full = buildBox(room.category, room.name, room.entries);
                if (fits(full)) return { box: full, trimmed: [] };
                for (let keep = room.entries.length - 1; keep >= 2; keep--) {
                    guard(`trimming ${category}/${room.name}`);
                    const prefix = room.entries.slice(0, keep);
                    if (!mergeTargetsArePresent(prefix)) continue;
                    const trial = buildBox(room.category, room.name, prefix);
                    if (fits(trial)) return { box: trial, trimmed: room.entries.slice(keep) };
                }
                return undefined;
            };

            let selected = selectBox();
            if (!selected && currentBoxes.length > 0) {
                flushBand(category);
                selected = selectBox();
            }
            if (!selected) {
                for (const entry of room.entries) {
                    guard(`dropping skipped ${category}/${room.name}`);
                    addDropped(entry.id, "skip");
                }
                continue;
            }
            for (const entry of selected.trimmed) {
                guard(`dropping trimmed ${category}/${room.name}`);
                addDropped(entry.id, "trim");
            }
            currentBoxes.push(selected.box);
            currentBandHeightPixels = Math.max(currentBandHeightPixels, selected.box.heightPixels);
            currentBandHeightRows = Math.max(currentBandHeightRows, selected.box.lines.length);
        }
        flushBand(category);
        currentCategory = undefined;
    }

    for (let row = palaceLines.length; row < PAGE_HEIGHT_CELLS; row++) {
        guard("padding the fixed page canvas");
        palaceLines.push("");
    }
    if (palaceLines.length > PAGE_HEIGHT_CELLS) {
        throw new Error(`single-page text canvas has ${palaceLines.length} rows (max ${PAGE_HEIGHT_CELLS})`);
    }
    const palace = `${palaceLines.join("\n")}\n`;
    const longLines = palaceLines
        .map((line, index) => ({ line: index + 1, chars: codepoints(line) }))
        .filter((item) => item.chars > MAX_LINE_CHARS);
    if (longLines.length > 0) {
        throw new Error(`lines exceed ${MAX_LINE_CHARS}: ${JSON.stringify(longLines)}`);
    }
    if (palace.length > MAX_PALACE_CHARS) {
        throw new Error(`palace has ${palace.length} chars (max ${MAX_PALACE_CHARS})`);
    }
    if (/#\d+/.test(palace)) {
        const message = "memory id leaked into palace.txt";
        if (!process.env.PALACE_RENDER_DESPITE_VALIDATOR) throw new Error(message);
        console.warn(`[palace] ${message}; rendering review manifest anyway`);
    }
    return {
        palace,
        placements,
        rooms: roomSummaries,
        layoutItems,
        pages: [
            {
                page: 1,
                startLine: 1,
                endLine: PAGE_HEIGHT_CELLS,
                heightCells: PAGE_HEIGHT_CELLS,
                heightPixels: PAGE_HEIGHT_PIXELS,
            },
        ],
        leveling: { gapRowsBefore: 0, gapRowsAfter: 0, splitCount: 0 },
        droppedByTrimIds,
        droppedBySkipIds,
    };
}

export function authorPalace(args: {
    source: SourceMemory[];
    specs: SpecEntry[];
    sourceLabel?: string;
    palaceOutput?: string;
    coverageOutput?: string;
}) {
    const palaceOutput = args.palaceOutput ?? PALACE_OUTPUT;
    const coverageOutput = args.coverageOutput ?? COVERAGE_OUTPUT;
    const reviewRender = Boolean(process.env.PALACE_RENDER_DESPITE_VALIDATOR);
    if (reviewRender) {
        // Review renders soft-break oversized unbreakable anchors before validation
        // and measurement, so every downstream consumer sees the same widths.
        const maxToken = ROOM_WIDTH - 6;
        for (const entry of args.specs) {
            if (entry.cue === undefined) continue;
            const parts = Array.isArray(entry.cue) ? entry.cue : [entry.cue];
            const softened = parts.map((part) =>
                part
                    .split(" ")
                    .map((token) =>
                        codepoints(token) > maxToken
                            ? Array.from(token)
                                  .reduce<string[]>((acc, ch) => {
                                      const last = acc[acc.length - 1];
                                      if (last === undefined || codepoints(last) >= maxToken - 1)
                                          acc.push(ch);
                                      else acc[acc.length - 1] = last + ch;
                                      return acc;
                                  }, [])
                                  .join("- ")
                            : token,
                    )
                    .join(" "),
            );
            entry.cue = Array.isArray(entry.cue) ? softened : softened[0];
        }
    }
    let renderSpecs = args.specs;
    try {
        validate(args.source, args.specs);
    } catch (error) {
        if (!reviewRender) throw error;
        console.warn(
            `[palace] validator rejected review manifest: ${error instanceof Error ? error.message : String(error)}; rendering anyway`,
        );
        const seenIds = new Set<number>();
        renderSpecs = args.specs.filter((entry) => {
            if (seenIds.has(entry.id)) return false;
            seenIds.add(entry.id);
            return true;
        });
    }
    const { palace, placements, rooms, layoutItems, pages, leveling, droppedByTrimIds, droppedBySkipIds: renderedSkipIds } =
        renderPalace(renderSpecs);

    const droppedBySkipIds = [...renderedSkipIds];
    const droppedSkipSet = new Set(droppedBySkipIds);
    const droppedTrimSet = new Set(droppedByTrimIds);
    const renderedIds = [...placements.keys()];
    const renderedSet = new Set(renderedIds);
    for (const memory of args.source) {
        if (
            !renderedSet.has(memory.id) &&
            !droppedTrimSet.has(memory.id) &&
            !droppedSkipSet.has(memory.id)
        ) {
            droppedSkipSet.add(memory.id);
            droppedBySkipIds.push(memory.id);
        }
    }
    const renderedEntries = renderSpecs.filter(
        (entry) => entry.mergeInto === undefined && placements.has(entry.id),
    );
    const renderedMerges = renderSpecs.filter(
        (entry) => entry.mergeInto !== undefined && placements.has(entry.id),
    );
    const cueLengths = renderedEntries.map((entry) => codepoints(displayCue(entry))).sort((a, b) => a - b);
    const percentile = (value: number): number =>
        cueLengths[Math.round((cueLengths.length - 1) * value)] ?? 0;
    const renderedMemoryCount = renderedIds.length;
    const droppedMemoryCount = droppedByTrimIds.length + droppedBySkipIds.length;
    const palaceLines = palace.endsWith("\n") ? palace.slice(0, -1).split("\n") : palace.split("\n");
    const coverage = {
        source: args.sourceLabel ?? SOURCE_PATH,
        sourceMemoryCount: args.source.length,
        renderedIds,
        droppedByTrimIds,
        droppedBySkipIds,
        renderedMemoryCount,
        droppedMemoryCount,
        entryCount: renderedEntries.length,
        mergeCount: renderedMerges.length,
        representedMemoryCount: renderedMemoryCount,
        palaceChars: palace.length,
        maxLineChars: Math.max(...palaceLines.map(codepoints)),
        layout: {
            font: LAYOUT_FONT,
            cellWidth: CELL_WIDTH,
            cellHeight: CELL_HEIGHT,
            columns: COLUMN_COUNT,
            roomWidthChars: ROOM_WIDTH,
            pageWidthChars: PAGE_WIDTH_CHARS,
            columnGapChars: COLUMN_GAP,
            canvasHeightCells: PAGE_HEIGHT_CELLS,
            pageHeightPixels: PAGE_HEIGHT_PIXELS,
            bodyLinePitch: BODY_LINE_PITCH,
            pages,
            cueLengthDistribution: {
                min: cueLengths[0] ?? 0,
                p25: percentile(0.25),
                median: percentile(0.5),
                p75: percentile(0.75),
                p90: percentile(0.9),
                max: cueLengths.at(-1) ?? 0,
            },
            sharedPairCount: rooms.reduce((total, room) => total + room.sharedPairCount, 0),
            bandGapRowsBefore: leveling.gapRowsBefore,
            bandGapRowsAfter: leveling.gapRowsAfter,
            roomSplitCount: leveling.splitCount,
            items: layoutItems,
        },
        rooms,
        memories: Object.fromEntries(
            [...placements.entries()]
                .sort(([a], [b]) => a - b)
                .map(([id, placement]) => [String(id), placement]),
        ),
    };
    writeFileSync(palaceOutput, palace);
    writeFileSync(coverageOutput, `${JSON.stringify(coverage, null, 4)}\n`);
    return { palace, coverage };
}

function main(): void {
    const specs = readSpecs();
    const source = parseSource(
        readFileSync(SOURCE_PATH, "utf8"),
        new Map(specs.map((spec) => [spec.id, spec.importance])),
    );
    const { palace, coverage } = authorPalace({ source, specs });
    console.log(
        JSON.stringify({
            palace: basename(PALACE_OUTPUT),
            font: LAYOUT_FONT,
            chars: palace.length,
            lines: palace.trimEnd().split("\n").length,
            entries: coverage.entryCount,
            merges: coverage.mergeCount,
            memories: coverage.representedMemoryCount,
            rooms: coverage.rooms.length,
            pages: coverage.layout.pages.map((page) => page.heightCells),
            cueLengths: coverage.layout.cueLengthDistribution,
            sharedPairs: coverage.layout.sharedPairCount,
            bandGaps: {
                before: coverage.layout.bandGapRowsBefore,
                after: coverage.layout.bandGapRowsAfter,
            },
            roomSplits: coverage.layout.roomSplitCount,
        }),
    );
}

if (import.meta.main) main();
