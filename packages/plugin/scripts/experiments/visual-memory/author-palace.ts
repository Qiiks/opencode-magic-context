import { readdirSync, readFileSync, writeFileSync } from "node:fs";
import { basename, dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const CATEGORY_ORDER = [
    "PROJECT_RULES",
    "ARCHITECTURE",
    "CONSTRAINTS",
    "CONFIG_VALUES",
    "NAMING",
    "KNOWN_ISSUES",
] as const;
const BAND_GROUPS: ReadonlyArray<ReadonlyArray<(typeof CATEGORY_ORDER)[number]>> = [
    ["PROJECT_RULES"],
    ["ARCHITECTURE"],
    ["CONSTRAINTS"],
    ["CONFIG_VALUES"],
    ["NAMING", "KNOWN_ISSUES"],
];
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
const MAX_PALACE_CHARS = 70_000;
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
 * Cue-quality and reviewable coverage defects downgrade to warnings when
 * PALACE_RENDER_DESPITE_VALIDATOR is set so near-miss manifests still render
 * for human review. Other validation failures remain fatal.
 */
function reportCueDefect(message: string, defects: string[]): void {
    if (process.env.PALACE_RENDER_DESPITE_VALIDATOR) {
        defects.push(message);
        console.warn(`[palace] cue defect (rendering anyway): ${message}`);
        return;
    }
    throw new Error(message);
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
            reportCueDefect(`duplicate spec id ${spec.id}`, defects);
            // First occurrence wins on review renders; a duplicate would otherwise
            // draw the same memory in two rooms and distort utilization numbers.
            continue;
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
        if (cue && /#\d+/.test(cue)) reportCueDefect(`memory id leaked into cue ${spec.id}`, defects);
        if (cue) {
            const renderedCue = displayCue(spec);
            const cueBudget = memory.importance >= 70 ? 90 : 50;
            const renderedCueLength = codepoints(renderedCue);
            if (renderedCueLength > cueBudget) {
                reportCueDefect(
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
                reportCueDefect(
                    `negative rule missing polarity marker in cue ${spec.id}: ${renderedCue}`,
                    defects,
                );
            }
            const polarityCount = mechanismCue.split("⊘").length - 1;
            const mechanismCount = mechanismCue.match(/\([^()]+\)/g)?.length ?? 0;
            if (polarityCount > mechanismCount) {
                reportCueDefect(
                    `polarity mechanism missing from rendered cue ${spec.id}: ${renderedCue}`,
                    defects,
                );
            }
            let marker = mechanismCue.indexOf("⊘");
            while (marker >= 0) {
                const nextMarker = mechanismCue.indexOf("⊘", marker + 1);
                const mechanism = mechanismCue.indexOf("(", marker + 1);
                if (mechanism < 0 || (nextMarker >= 0 && mechanism > nextMarker)) {
                    reportCueDefect(
                        `polarity mechanism must follow marker ${spec.id}: ${renderedCue}`,
                        defects,
                    );
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
    const missing = source.filter((memory) => !specById.has(memory.id));
    if (missing.length > 0)
        reportCueDefect(`uncovered source ids: ${missing.map((item) => item.id).join(", ")}`, defects);
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
    const entries = allEntries
        .filter((entry) => entry.mergeInto === undefined)
        .sort((a, b) => a.id - b.id);
    const merges = allEntries
        .filter((entry) => entry.mergeInto !== undefined)
        .sort((a, b) => a.id - b.id);
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

function segmentBox(
    box: Box,
    start: number,
    end: number,
    continuation: boolean,
    segment: number,
): Box {
    const bodyLines = box.bodyLines.slice(start, end);
    const entries = box.entries.filter((entry) => {
        const line = box.entryBodyLines.get(entry.id);
        return line !== undefined && line >= start && line < end;
    });
    const entryIds = new Set(entries.map((entry) => entry.id));
    const merges = box.merges.filter(
        (merge) => merge.mergeInto !== undefined && entryIds.has(merge.mergeInto),
    );
    const entryBodyLines = new Map<number, number>();
    const relativeLines = new Map<number, number>();
    const headerLines = continuation ? 1 : 2;
    for (const entry of entries) {
        const originalLine = box.entryBodyLines.get(entry.id);
        if (originalLine === undefined) throw new Error(`split body line missing for ${entry.id}`);
        const bodyLine = originalLine - start;
        entryBodyLines.set(entry.id, bodyLine);
        relativeLines.set(entry.id, bodyLine + headerLines);
    }
    for (const merge of merges) {
        const targetLine =
            merge.mergeInto === undefined ? undefined : relativeLines.get(merge.mergeInto);
        if (targetLine === undefined) throw new Error(`split merge target missing for ${merge.id}`);
        relativeLines.set(merge.id, targetLine);
    }
    return {
        ...box,
        lines: frameBox(box.name, bodyLines, box.peakImportance, continuation),
        bodyLines,
        entryBodyLines,
        relativeLines,
        entries,
        merges,
        sharedPairCount: entries.length - new Set(entryBodyLines.values()).size,
        continuation,
        segment,
        heightPixels:
            (continuation ? 2 * CELL_HEIGHT : 3 * CELL_HEIGHT) + bodyLines.length * BODY_LINE_PITCH,
    };
}

function splitBox(box: Box): [Box, Box] | undefined {
    const boundaries = [
        ...new Set(box.entries.map((entry) => box.entryBodyLines.get(entry.id) ?? 0)),
    ]
        .filter((line) => line > 0 && line < box.bodyLines.length)
        .sort((a, b) => a - b);
    if (boundaries.length === 0) return undefined;
    const midpoint = box.bodyLines.length / 2;
    const boundary = boundaries.reduce((best, candidate) =>
        Math.abs(candidate - midpoint) < Math.abs(best - midpoint) ? candidate : best,
    );
    return [
        segmentBox(box, 0, boundary, box.continuation, box.segment),
        segmentBox(box, boundary, box.bodyLines.length, true, box.segment + 1),
    ];
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
} {
    const grouped = new Map<string, SpecEntry[]>();
    for (const spec of specs) {
        const key = `${spec.category}\u0000${spec.room}`;
        const list = grouped.get(key) ?? [];
        list.push(spec);
        grouped.set(key, list);
    }

    const MAX_EXACT_LAYOUT_BOXES = 12;
    const MAX_LAYOUT_SEARCH_ITERATIONS = 1_000_000;
    const assignmentFor = (boxes: Box[], context: string): number[] => {
        // Exhaustively comparing every three-column assignment is useful for normal
        // manifests, but its search space is exponential. Review renders must also
        // survive malformed manifests that put many one-entry rooms in one category.
        if (boxes.length > MAX_EXACT_LAYOUT_BOXES) {
            const columns = Array<number>(boxes.length).fill(0);
            const heights = Array<number>(COLUMN_COUNT).fill(0);
            for (const [index, box] of boxes.entries()) {
                const column = index === 0 ? 0 : heights.indexOf(Math.min(...heights));
                columns[index] = column;
                heights[column] += box.heightPixels;
            }
            return columns;
        }
        let best: { columns: number[]; max: number; range: number; rowRange: number } | undefined;
        const columns = Array<number>(boxes.length).fill(0);
        const heights = Array<number>(COLUMN_COUNT).fill(0);
        const rowHeights = Array<number>(COLUMN_COUNT).fill(0);
        let iterations = 0;
        const visit = (index: number): void => {
            if (++iterations > MAX_LAYOUT_SEARCH_ITERATIONS) {
                throw new Error(
                    `masonry assignment exceeded ${MAX_LAYOUT_SEARCH_ITERATIONS} iterations while placing ${context}`,
                );
            }
            if (index === boxes.length) {
                const max = Math.max(...heights);
                const range = max - Math.min(...heights);
                const rowRange = Math.max(...rowHeights) - Math.min(...rowHeights);
                if (
                    !best ||
                    max < best.max ||
                    (max === best.max && rowRange < best.rowRange) ||
                    (max === best.max && rowRange === best.rowRange && range < best.range)
                ) {
                    best = { columns: [...columns], max, range, rowRange };
                }
                return;
            }
            const box = boxes[index];
            if (!box) return;
            const lastColumn = index === 0 ? 1 : COLUMN_COUNT;
            for (let column = 0; column < lastColumn; column++) {
                columns[index] = column;
                heights[column] += box.heightPixels;
                rowHeights[column] += box.lines.length;
                if (!best || Math.max(...heights) <= best.max) visit(index + 1);
                heights[column] -= box.heightPixels;
                rowHeights[column] -= box.lines.length;
            }
        };
        visit(0);
        if (!best) throw new Error(`unable to assign masonry band while placing ${context}`);
        return best.columns;
    };

    const levelBand = (
        inputBoxes: Box[],
        inputAssignment: number[],
    ): {
        boxes: Box[];
        assignment: number[];
        gapBefore: number;
        gapAfter: number;
        splits: number;
    } => {
        const boxes = [...inputBoxes];
        const assignment = [...inputAssignment];
        const heights = (): number[] => {
            const result = Array<number>(COLUMN_COUNT).fill(0);
            for (const [index, box] of boxes.entries())
                result[assignment[index] ?? 0] += box.lines.length;
            return result;
        };
        const initialHeights = heights();
        const gapBefore = Math.max(...initialHeights) - Math.min(...initialHeights);
        let splits = 0;
        while (splits < COLUMN_COUNT) {
            const current = heights();
            const tallest = current.indexOf(Math.max(...current));
            const shortest = current.indexOf(Math.min(...current));
            const gap = current[tallest] - current[shortest];
            if (gap <= 4) break;
            let splitIndex = -1;
            for (let index = boxes.length - 1; index >= 0; index--) {
                if (assignment[index] === tallest) {
                    splitIndex = index;
                    break;
                }
            }
            const candidate = splitIndex >= 0 ? boxes[splitIndex] : undefined;
            const split = candidate ? splitBox(candidate) : undefined;
            if (!candidate || !split) break;
            const [first, continuation] = split;
            const nextHeights = [...current];
            nextHeights[tallest] += first.lines.length - candidate.lines.length;
            nextHeights[shortest] += continuation.lines.length;
            const nextGap = Math.max(...nextHeights) - Math.min(...nextHeights);
            if (nextGap >= gap) break;
            boxes.splice(splitIndex, 1, first, continuation);
            assignment.splice(splitIndex, 1, tallest, shortest);
            splits++;
        }
        const finalHeights = heights();
        return {
            boxes,
            assignment,
            gapBefore,
            gapAfter: Math.max(...finalHeights) - Math.min(...finalHeights),
            splits,
        };
    };

    const subsetForCapacity = (
        boxes: Box[],
        capacity: number,
        context: string,
    ): { indexes: number[]; columns: number[] } => {
        let best:
            | {
                  indexes: number[];
                  columns: number[];
                  remainderMax: number;
                  selectedMax: number;
                  remainderRange: number;
                  selectedHeight: number;
                  maxRowGap: number;
                  totalRowGap: number;
              }
            | undefined;
        if (boxes.length > MAX_EXACT_LAYOUT_BOXES) {
            const indexes: number[] = [];
            const selected: Box[] = [];
            const candidates = boxes
                .map((box, index) => ({ box, index }))
                .sort((a, b) => a.box.heightPixels - b.box.heightPixels);
            for (const candidate of candidates) {
                const trial = [...selected, candidate.box];
                const trialColumns = assignmentFor(trial, context);
                const trialLeveled = levelBand(trial, trialColumns);
                const trialHeights = Array<number>(COLUMN_COUNT).fill(0);
                for (const [index, box] of trialLeveled.boxes.entries()) {
                    trialHeights[trialLeveled.assignment[index] ?? 0] += box.heightPixels;
                }
                if (Math.max(...trialHeights) <= capacity) {
                    indexes.push(candidate.index);
                    selected.push(candidate.box);
                }
            }
            if (indexes.length === 0) {
                throw new Error(`no room fits ${capacity}-row page while placing ${context}`);
            }
            return { indexes, columns: assignmentFor(selected, context) };
        }
        const limit = 2 ** boxes.length;
        let iterations = 0;
        for (let mask = 1; mask < limit - 1; mask++) {
            if (++iterations > MAX_LAYOUT_SEARCH_ITERATIONS) {
                throw new Error(
                    `masonry subset search exceeded ${MAX_LAYOUT_SEARCH_ITERATIONS} iterations while placing ${context}`,
                );
            }
            const indexes = boxes
                .map((_, index) => index)
                .filter((index) => (mask & (1 << index)) !== 0);
            const selected = indexes
                .map((index) => boxes[index])
                .filter((box): box is Box => Boolean(box));
            const remainder = boxes.filter((_, index) => (mask & (1 << index)) === 0);
            const columns = assignmentFor(selected, context);
            const selectedLeveled = levelBand(selected, columns);
            const selectedHeights = Array<number>(COLUMN_COUNT).fill(0);
            for (const [index, box] of selectedLeveled.boxes.entries()) {
                selectedHeights[selectedLeveled.assignment[index] ?? 0] += box.heightPixels;
            }
            const selectedMax = Math.max(...selectedHeights);
            if (selectedMax > capacity) continue;
            const remainderColumns = assignmentFor(remainder, context);
            const remainderLeveled = levelBand(remainder, remainderColumns);
            const remainderHeights = Array<number>(COLUMN_COUNT).fill(0);
            for (const [index, box] of remainderLeveled.boxes.entries()) {
                remainderHeights[remainderLeveled.assignment[index] ?? 0] += box.heightPixels;
            }
            const remainderMax = Math.max(...remainderHeights);
            const remainderRange = remainderMax - Math.min(...remainderHeights);
            const selectedHeight = selectedLeveled.boxes.reduce(
                (total, box) => total + box.heightPixels,
                0,
            );
            const selectedRowGap = selectedLeveled.gapAfter;
            const remainderRowGap = remainderLeveled.gapAfter;
            const maxRowGap = Math.max(selectedRowGap, remainderRowGap);
            const totalRowGap = selectedRowGap + remainderRowGap;
            if (
                !best ||
                maxRowGap < best.maxRowGap ||
                (maxRowGap === best.maxRowGap && totalRowGap < best.totalRowGap) ||
                (maxRowGap === best.maxRowGap &&
                    totalRowGap === best.totalRowGap &&
                    remainderMax < best.remainderMax) ||
                (maxRowGap === best.maxRowGap &&
                    totalRowGap === best.totalRowGap &&
                    remainderMax === best.remainderMax &&
                    selectedMax > best.selectedMax) ||
                (maxRowGap === best.maxRowGap &&
                    totalRowGap === best.totalRowGap &&
                    remainderMax === best.remainderMax &&
                    selectedMax === best.selectedMax &&
                    remainderRange < best.remainderRange) ||
                (maxRowGap === best.maxRowGap &&
                    totalRowGap === best.totalRowGap &&
                    remainderMax === best.remainderMax &&
                    selectedMax === best.selectedMax &&
                    remainderRange === best.remainderRange &&
                    selectedHeight > best.selectedHeight)
            ) {
                best = {
                    indexes,
                    columns,
                    remainderMax,
                    selectedMax,
                    remainderRange,
                    selectedHeight,
                    maxRowGap,
                    totalRowGap,
                };
            }
        }
        if (!best) throw new Error(`no room fits ${capacity}-row page remainder`);
        return { indexes: best.indexes, columns: best.columns };
    };

    const palaceLines: string[] = [];
    const pageLines: string[][] = [[]];
    const pagePixelHeights: number[] = [0];
    const placements = new Map<number, Placement>();
    const roomSummaries: RoomSummary[] = [];
    const layoutItems: LayoutItem[] = [];
    let gapRowsBefore = 0;
    let gapRowsAfter = 0;
    let splitCount = 0;
    const categoryBanner = (categories: readonly Category[], continued: boolean): string => {
        const label = ` <${categories.join(" + ")}${continued ? " CONT." : ""}> `;
        const remaining = PAGE_WIDTH_CHARS - codepoints(label);
        return `${"─".repeat(Math.floor(remaining / 2))}${label}${"─".repeat(Math.ceil(remaining / 2))}`;
    };

    for (const categories of BAND_GROUPS) {
        const primaryCategory = categories[0];
        if (!primaryCategory) throw new Error("empty band group");
        let remaining = [...grouped.entries()]
            .filter(([, entries]) => {
                const category = entries[0]?.category;
                return category ? categories.includes(category) : false;
            })
            .map(([key, entries]) => {
                const category = entries[0]?.category;
                if (!category) throw new Error(`empty room group ${key}`);
                return buildBox(category, key.slice(category.length + 1), entries);
            })
            .sort((a, b) => {
                const categoryOrder =
                    CATEGORY_ORDER.indexOf(a.category) - CATEGORY_ORDER.indexOf(b.category);
                if (categoryOrder !== 0) return categoryOrder;
                return a.name < b.name ? -1 : a.name > b.name ? 1 : 0;
            });
        let continued = false;
        let packIterations = 0;
        const maxPackIterations = Math.max(
            64,
            remaining.reduce((total, box) => total + box.bodyLines.length, 0) * 4,
        );
        while (remaining.length > 0) {
            const pendingBox = remaining[0];
            if (!pendingBox) throw new Error("page packer lost a pending room");
            const placementContext = `${pendingBox.category}/${pendingBox.name} segment ${pendingBox.segment} entry ${pendingBox.entries[0]?.id ?? "none"}`;
            if (++packIterations > maxPackIterations) {
                throw new Error(
                    `page packer exceeded ${maxPackIterations} iterations while placing room ${pendingBox.category}/${pendingBox.name} segment ${pendingBox.segment}`,
                );
            }
            let pageIndex = pageLines.length - 1;
            let available =
                PAGE_HEIGHT_PIXELS - (pagePixelHeights[pageIndex] ?? 0) - BANNER_HEIGHT_PIXELS;
            if (available <= 0) {
                pageLines.push([]);
                pagePixelHeights.push(0);
                pageIndex++;
                available = PAGE_HEIGHT_PIXELS - BANNER_HEIGHT_PIXELS;
            }
            const smallestBox = remaining.reduce((smallest, box) =>
                box.heightPixels < smallest.heightPixels ? box : smallest,
            );
            if (smallestBox.heightPixels > available) {
                if ((pagePixelHeights[pageIndex] ?? 0) > 0) {
                    pageLines.push([]);
                    pagePixelHeights.push(0);
                    continue;
                }
                const split = splitBox(smallestBox);
                if (!split) {
                    throw new Error(
                        `room ${smallestBox.category}/${smallestBox.name} segment ${smallestBox.segment} is ${smallestBox.heightPixels}px and cannot split into an ${available}px page`,
                    );
                }
                const [first, continuation] = split;
                if (
                    first.heightPixels >= smallestBox.heightPixels ||
                    continuation.heightPixels >= smallestBox.heightPixels
                ) {
                    throw new Error(
                        `room ${smallestBox.category}/${smallestBox.name} segment ${smallestBox.segment} did not shrink when split for an ${available}px page`,
                    );
                }
                const splitIndex = remaining.indexOf(smallestBox);
                if (splitIndex < 0) throw new Error("page packer lost its oversized room");
                remaining.splice(splitIndex, 1, first, continuation);
                splitCount++;
                continue;
            }
            const remainingCount = remaining.length;
            const fullAssignment = assignmentFor(remaining, placementContext);
            const fullHeights = Array<number>(COLUMN_COUNT).fill(0);
            for (const [index, box] of remaining.entries()) {
                fullHeights[fullAssignment[index] ?? 0] += box.heightPixels;
            }
            let segmentBoxes: Box[];
            let assignment: number[];
            let selectedIndexes: number[];
            if (Math.max(...fullHeights) <= available) {
                segmentBoxes = remaining;
                assignment = fullAssignment;
                selectedIndexes = remaining.map((_, index) => index);
            } else {
                const subset = subsetForCapacity(remaining, available, placementContext);
                selectedIndexes = subset.indexes;
                segmentBoxes = selectedIndexes
                    .map((index) => remaining[index])
                    .filter((box): box is Box => Boolean(box));
                assignment = subset.columns;
            }

            const leveled = levelBand(segmentBoxes, assignment);
            segmentBoxes = leveled.boxes;
            assignment = leveled.assignment;
            gapRowsBefore += leveled.gapBefore;
            gapRowsAfter += leveled.gapAfter;
            splitCount += leveled.splits;

            const columns = Array.from({ length: COLUMN_COUNT }, () => [] as string[]);
            const heights = Array<number>(COLUMN_COUNT).fill(0);
            const pixelHeights = Array<number>(COLUMN_COUNT).fill(0);
            for (const [index, box] of segmentBoxes.entries()) {
                const column = assignment[index] ?? 0;
                columns[column].push(...box.lines);
                heights[column] += box.lines.length;
                pixelHeights[column] += box.heightPixels;
            }
            const bandHeight = Math.max(...heights);
            const bandHeightPixels = Math.max(...pixelHeights);
            if (bandHeightPixels > available)
                throw new Error(`leveled band exceeds page by ${bandHeightPixels - available}px`);
            const page = pageIndex + 1;
            const pageLine = (pageLines[pageIndex]?.length ?? 0) + 1;
            const bannerLine = palaceLines.length + 1;
            const bannerTopPixels = pagePixelHeights[pageIndex] ?? 0;
            const banner = categoryBanner(categories, continued);
            palaceLines.push(banner);
            pageLines[pageIndex]?.push(banner);
            layoutItems.push({
                kind: "category",
                category: primaryCategory,
                categories: [...categories],
                column: 0,
                startLine: bannerLine,
                endLine: bannerLine,
                page,
                pageLine,
                pageTopPixels: bannerTopPixels,
                heightPixels: BANNER_HEIGHT_PIXELS,
            });

            const bandStartLine = palaceLines.length + 1;
            const bandPageLine = (pageLines[pageIndex]?.length ?? 0) + 1;
            const columnRows = Array<number>(COLUMN_COUNT).fill(0);
            const columnPixelRows = Array<number>(COLUMN_COUNT).fill(0);
            for (const [index, box] of segmentBoxes.entries()) {
                const column = assignment[index] ?? 0;
                const row = columnRows[column] ?? 0;
                columnRows[column] = row + box.lines.length;
                const pixelRow = columnPixelRows[column] ?? 0;
                columnPixelRows[column] = pixelRow + box.heightPixels;
                const startLine = bandStartLine + row;
                const roomPageLine = bandPageLine + row;
                const roomPageTopPixels = bannerTopPixels + BANNER_HEIGHT_PIXELS + pixelRow;
                for (const entry of [...box.entries, ...box.merges]) {
                    const relativeLine = box.relativeLines.get(entry.id);
                    if (relativeLine === undefined)
                        throw new Error(`placement missing for ${entry.id}`);
                    placements.set(entry.id, {
                        category: box.category,
                        room: box.name,
                        palaceLine: startLine + relativeLine,
                        palaceColumn: column * (ROOM_WIDTH + COLUMN_GAP) + 1,
                        page,
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
                    continuation: box.continuation,
                    segment: box.segment,
                    page,
                    pageLine: roomPageLine,
                    pageTopPixels: roomPageTopPixels,
                    heightPixels: box.heightPixels,
                });
                layoutItems.push({
                    kind: "room",
                    category: box.category,
                    room: box.name,
                    continuation: box.continuation,
                    segment: box.segment,
                    column,
                    startLine,
                    endLine: startLine + box.lines.length - 1,
                    page,
                    pageLine: roomPageLine,
                    pageTopPixels: roomPageTopPixels,
                    heightPixels: box.heightPixels,
                });
            }

            for (let row = 0; row < bandHeight; row++) {
                const line = columns
                    .map((column) => (column[row] ?? "").padEnd(ROOM_WIDTH))
                    .join(" ".repeat(COLUMN_GAP))
                    .trimEnd();
                palaceLines.push(line);
                pageLines[pageIndex]?.push(line);
            }

            pagePixelHeights[pageIndex] = bannerTopPixels + BANNER_HEIGHT_PIXELS + bandHeightPixels;

            const selected = new Set(selectedIndexes);
            const nextRemaining = remaining.filter((_, index) => !selected.has(index));
            if (nextRemaining.length >= remainingCount) {
                throw new Error(
                    `page packer made no progress while placing room ${pendingBox.category}/${pendingBox.name} segment ${pendingBox.segment}`,
                );
            }
            remaining = nextRemaining;
            // Keep the page open; the next band creates a page only when its banner and
            // boxes cannot fit.
            if (remaining.length > 0) continued = true;
        }
    }

    const palace = `${palaceLines.join("\n")}\n`;
    const longLines = palaceLines
        .map((line, index) => ({ line: index + 1, chars: codepoints(line) }))
        .filter((item) => item.chars > MAX_LINE_CHARS);
    if (longLines.length > 0) {
        throw new Error(`lines exceed ${MAX_LINE_CHARS}: ${JSON.stringify(longLines)}`);
    }
    if (palace.length > MAX_PALACE_CHARS) {
        const message = `palace has ${palace.length} chars (max ${MAX_PALACE_CHARS})`;
        if (!process.env.PALACE_RENDER_DESPITE_VALIDATOR) throw new Error(message);
        console.warn(`[palace] ${message}; rendering review manifest anyway`);
    }
    if (/#\d+/.test(palace)) {
        const message = "memory id leaked into palace.txt";
        if (!process.env.PALACE_RENDER_DESPITE_VALIDATOR) throw new Error(message);
        console.warn(`[palace] ${message}; rendering review manifest anyway`);
    }
    let startLine = 1;
    const pages = pageLines.map((lines, index) => {
        const page = {
            page: index + 1,
            startLine,
            endLine: startLine + lines.length - 1,
            heightCells: lines.length,
            heightPixels: pagePixelHeights[index] ?? 0,
        };
        startLine += lines.length;
        return page;
    });
    return {
        palace,
        placements,
        rooms: roomSummaries,
        layoutItems,
        pages,
        leveling: { gapRowsBefore, gapRowsAfter, splitCount },
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
        // Review renders soft-break oversized unbreakable anchors BEFORE any
        // validation or box measurement, so every downstream consumer (width
        // checks, box geometry, the page packer) sees the same widths. Breaking
        // lazily inside the line wrapper instead once left box geometry measured
        // from the raw token, and the packer looped forever on a box wider than
        // its page. Real runs stay fatal so the authoring prompt owns budgets.
        const maxToken = ROOM_WIDTH - 6;
        for (const entry of args.specs) {
            if (entry.cue === undefined) continue;
            const parts = Array.isArray(entry.cue) ? entry.cue : [entry.cue];
            const softened = parts.map((part) =>
                part
                    .split(" ")
                    .map((token) =>
                        codepoints(token) > maxToken
                            ? (Array.from(token)
                                  .reduce<string[]>((acc, ch) => {
                                      const last = acc[acc.length - 1];
                                      if (last === undefined || codepoints(last) >= maxToken - 1)
                                          acc.push(ch);
                                      else acc[acc.length - 1] = last + ch;
                                      return acc;
                                  }, [])
                                  .join("- ") ?? token)
                            : token,
                    )
                    .join(" "),
            );
            entry.cue = Array.isArray(entry.cue) ? softened : softened[0];
        }
    }
    validate(args.source, args.specs);
    let renderSpecs = args.specs;
    if (reviewRender) {
        // Validation keeps the first duplicate id, so the review layout must do
        // the same instead of drawing one memory in multiple rooms.
        const renderedIds = new Set<number>();
        renderSpecs = args.specs.filter((entry) => {
            if (renderedIds.has(entry.id)) return false;
            renderedIds.add(entry.id);
            return true;
        });
    }
    const { palace, placements, rooms, layoutItems, pages, leveling } = renderPalace(renderSpecs);
    const cueLengths = renderSpecs
        .filter((entry) => entry.mergeInto === undefined)
        .map((entry) => codepoints(displayCue(entry)))
        .sort((a, b) => a - b);
    const percentile = (value: number): number =>
        cueLengths[Math.round((cueLengths.length - 1) * value)] ?? 0;
    const entryCount = renderSpecs.filter((entry) => entry.mergeInto === undefined).length;
    const mergeCount = renderSpecs.length - entryCount;
    const coverage = {
        source: args.sourceLabel ?? SOURCE_PATH,
        sourceMemoryCount: args.source.length,
        entryCount,
        mergeCount,
        representedMemoryCount: entryCount + mergeCount,
        palaceChars: palace.length,
        maxLineChars: Math.max(...palace.trimEnd().split("\n").map(codepoints)),
        layout: {
            font: LAYOUT_FONT,
            cellWidth: CELL_WIDTH,
            cellHeight: CELL_HEIGHT,
            columns: COLUMN_COUNT,
            roomWidthChars: ROOM_WIDTH,
            pageWidthChars: PAGE_WIDTH_CHARS,
            columnGapChars: COLUMN_GAP,
            canvasHeightCells: palace.trimEnd().split("\n").length,
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
    if (placements.size !== args.source.length) {
        const message = `coverage has ${placements.size}/${args.source.length}`;
        if (!reviewRender) throw new Error(message);
        console.warn(`[palace] ${message}; rendering review manifest anyway`);
    }
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
