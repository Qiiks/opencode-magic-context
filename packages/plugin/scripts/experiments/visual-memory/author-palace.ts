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

type Category = (typeof CATEGORY_ORDER)[number];
type SpecEntry = {
    id: number;
    category: Category;
    room: string;
    cue?: string | string[];
    mergeInto?: number;
    importance: number;
};
type SourceMemory = { id: number; category: Category };
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

function parseSource(source: string): SourceMemory[] {
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
            memories.push({ id: Number(id), category });
        }
    }
    return memories;
}

function readSpecs(): SpecEntry[] {
    const files = readdirSync(HERE)
        .filter((file) => file.startsWith("spec-") && file.endsWith(".json"))
        .sort();
    return files.flatMap(
        (file) => JSON.parse(readFileSync(join(HERE, file), "utf8")) as SpecEntry[],
    );
}

function isExactToken(value: string): boolean {
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

function validate(source: SourceMemory[], specs: SpecEntry[]): void {
    if (source.length !== 334)
        throw new Error(`expected 334 source memories, found ${source.length}`);
    const sourceById = new Map(source.map((memory) => [memory.id, memory]));
    const specById = new Map<number, SpecEntry>();
    for (const spec of specs) {
        if (specById.has(spec.id)) throw new Error(`duplicate spec id ${spec.id}`);
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
            for (const hubWord of spec.room
                .split(/[^A-Za-z0-9]+/)
                .filter((word) => word.length >= 2)) {
                if (new RegExp(`(?<![\\w/._-])${hubWord}(?![\\w/._-])`, "i").test(renderedCue)) {
                    throw new Error(`hub noun repeated in cue ${spec.id}: ${renderedCue}`);
                }
            }
            const negativeRule = /\b(?:must not|never|without|instead of|excludes?)\b/i.test(
                renderedCue,
            );
            if (negativeRule && !renderedCue.includes("⊘")) {
                throw new Error(
                    `negative rule missing polarity marker in cue ${spec.id}: ${renderedCue}`,
                );
            }
            const polarityCount = renderedCue.split("⊘").length - 1;
            const mechanismCount = renderedCue.match(/\([^()]+\)/g)?.length ?? 0;
            if (polarityCount > mechanismCount) {
                throw new Error(
                    `polarity mechanism missing from rendered cue ${spec.id}: ${renderedCue}`,
                );
            }
            let marker = renderedCue.indexOf("⊘");
            while (marker >= 0) {
                const nextMarker = renderedCue.indexOf("⊘", marker + 1);
                const mechanism = renderedCue.indexOf("(", marker + 1);
                if (mechanism < 0 || (nextMarker >= 0 && mechanism > nextMarker)) {
                    throw new Error(
                        `polarity mechanism must follow marker ${spec.id}: ${renderedCue}`,
                    );
                }
                let depth = 0;
                let close = -1;
                for (let index = mechanism; index < renderedCue.length; index++) {
                    if (renderedCue[index] === "(") depth++;
                    if (renderedCue[index] === ")") depth--;
                    if (depth === 0) {
                        close = index;
                        break;
                    }
                }
                if (close < mechanism) {
                    throw new Error(`polarity mechanism is unclosed ${spec.id}: ${renderedCue}`);
                }
                marker = renderedCue.indexOf("⊘", close + 1);
            }
            const unclosed = [...renderedCue].reduce(
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
        throw new Error(`uncovered source ids: ${missing.map((item) => item.id).join(", ")}`);
    for (const spec of specs) {
        if (spec.mergeInto === undefined) continue;
        const target = specById.get(spec.mergeInto);
        if (!target || target.mergeInto !== undefined)
            throw new Error(`invalid merge target ${spec.mergeInto}`);
        if (target.category !== spec.category || target.room !== spec.room) {
            throw new Error(`merge ${spec.id} crosses room/category`);
        }
    }
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

function renderPalace(specs: SpecEntry[]): {
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

    const assignmentFor = (boxes: Box[]): number[] => {
        let best: { columns: number[]; max: number; range: number; rowRange: number } | undefined;
        const columns = Array<number>(boxes.length).fill(0);
        const heights = Array<number>(COLUMN_COUNT).fill(0);
        const rowHeights = Array<number>(COLUMN_COUNT).fill(0);
        const visit = (index: number): void => {
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
        if (!best) throw new Error("unable to assign masonry band");
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
        const limit = 1 << boxes.length;
        for (let mask = 1; mask < limit - 1; mask++) {
            const indexes = boxes
                .map((_, index) => index)
                .filter((index) => (mask & (1 << index)) !== 0);
            const selected = indexes
                .map((index) => boxes[index])
                .filter((box): box is Box => Boolean(box));
            const remainder = boxes.filter((_, index) => (mask & (1 << index)) === 0);
            const columns = assignmentFor(selected);
            const selectedLeveled = levelBand(selected, columns);
            const selectedHeights = Array<number>(COLUMN_COUNT).fill(0);
            for (const [index, box] of selectedLeveled.boxes.entries()) {
                selectedHeights[selectedLeveled.assignment[index] ?? 0] += box.heightPixels;
            }
            const selectedMax = Math.max(...selectedHeights);
            if (selectedMax > capacity) continue;
            const remainderColumns = assignmentFor(remainder);
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
        while (remaining.length > 0) {
            let pageIndex = pageLines.length - 1;
            let available =
                PAGE_HEIGHT_PIXELS - (pagePixelHeights[pageIndex] ?? 0) - BANNER_HEIGHT_PIXELS;
            if (available <= 0) {
                pageLines.push([]);
                pagePixelHeights.push(0);
                pageIndex++;
                available = PAGE_HEIGHT_PIXELS - BANNER_HEIGHT_PIXELS;
            }
            if (Math.min(...remaining.map((box) => box.heightPixels)) > available) {
                pageLines.push([]);
                pagePixelHeights.push(0);
                continued = true;
                continue;
            }
            const fullAssignment = assignmentFor(remaining);
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
                const subset = subsetForCapacity(remaining, available);
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
            remaining = remaining.filter((_, index) => !selected.has(index));
            if (remaining.length > 0) {
                pageLines.push([]);
                pagePixelHeights.push(0);
                continued = true;
            }
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
        throw new Error(`palace has ${palace.length} chars (max ${MAX_PALACE_CHARS})`);
    }
    if (/#\d+/.test(palace)) throw new Error("memory id leaked into palace.txt");
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

const sourceText = readFileSync(SOURCE_PATH, "utf8");
const source = parseSource(sourceText);
const specs = readSpecs();
validate(source, specs);
const { palace, placements, rooms, layoutItems, pages, leveling } = renderPalace(specs);
const cueLengths = specs
    .filter((entry) => entry.mergeInto === undefined)
    .map((entry) => codepoints(displayCue(entry)))
    .sort((a, b) => a - b);
const percentile = (value: number): number =>
    cueLengths[Math.round((cueLengths.length - 1) * value)] ?? 0;
const entryCount = specs.filter((entry) => entry.mergeInto === undefined).length;
const mergeCount = specs.length - entryCount;
const coverage = {
    source: SOURCE_PATH,
    sourceMemoryCount: source.length,
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
if (placements.size !== source.length)
    throw new Error(`coverage has ${placements.size}/${source.length}`);
writeFileSync(PALACE_OUTPUT, palace);
writeFileSync(COVERAGE_OUTPUT, `${JSON.stringify(coverage, null, 4)}\n`);
console.log(
    JSON.stringify({
        palace: basename(PALACE_OUTPUT),
        font: LAYOUT_FONT,
        chars: palace.length,
        lines: palace.trimEnd().split("\n").length,
        entries: entryCount,
        merges: mergeCount,
        memories: placements.size,
        rooms: rooms.length,
        pages: pages.map((page) => page.heightCells),
        cueLengths: coverage.layout.cueLengthDistribution,
        sharedPairs: coverage.layout.sharedPairCount,
        bandGaps: { before: leveling.gapRowsBefore, after: leveling.gapRowsAfter },
        roomSplits: leveling.splitCount,
    }),
);
