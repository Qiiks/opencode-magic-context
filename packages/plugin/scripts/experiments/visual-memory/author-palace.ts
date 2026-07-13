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
const COLUMN_COUNT = 3;
const ROOM_WIDTH = 90;
const COLUMN_GAP = 2;
const MAX_LINE_CHARS = COLUMN_COUNT * ROOM_WIDTH + (COLUMN_COUNT - 1) * COLUMN_GAP;
const MAX_PALACE_CHARS = 50_000;
const SOURCE_PATH = "/tmp/visual-memory/trimmed-memories-source.txt";
const HERE = dirname(fileURLToPath(import.meta.url));

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
};
type LayoutItem = {
    kind: "category" | "room";
    category: Category;
    room?: string;
    column: number;
    startLine: number;
    endLine: number;
};

type Box = {
    category: Category;
    name: string;
    lines: string[];
    relativeLines: Map<number, number>;
    entries: SpecEntry[];
    merges: SpecEntry[];
    peakImportance: number;
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
    const protectedValues: string[] = [];
    let value = raw.replace(/`[^`]+`|[^\s()`]+/g, (match) => {
        if (!match.startsWith("`") && !isExactToken(match)) return match;
        const marker = `QZ${protectedValues.length}ZQ`;
        protectedValues.push(match);
        return marker;
    });
    const hubWords = room
        .split(/\s+(?:&|and)\s+|\s+/)
        .filter((word) => word.length >= 5)
        .map((word) => word.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"));
    if (hubWords.length > 0) {
        value = value.replace(new RegExp(`\\b(?:${hubWords.join("|")})\\b`, "gi"), "");
    }
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
            const unclosed = [...renderedCue].reduce(
                (depth, character) => depth + (character === "(" ? 1 : character === ")" ? -1 : 0),
                0,
            );
            if (unclosed !== 0)
                throw new Error(`unbalanced mechanism in rendered cue ${spec.id}: ${renderedCue}`);
            const exactAnchors = (cue.match(/`[^`]+`|[^\s()`]+/g) ?? []).filter(
                (anchor) => anchor.startsWith("`") || isExactToken(anchor),
            );
            for (const rawAnchor of exactAnchors) {
                const anchor = rawAnchor.replace(/^[,;]+|[,;]+$/g, "");
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
    let lineIndex = body.length - 1;
    let prefix = body[lineIndex]?.length ? "¦" : "•";
    const firstLine =
        lineIndex < 0 || codepoints(`${body[lineIndex]}${prefix}${words[0] ?? ""}`) > width;
    if (firstLine) {
        body.push("");
        lineIndex = body.length - 1;
        prefix = "•";
    }
    const placement = lineIndex;
    for (const word of words) {
        const separator =
            prefix || body[lineIndex].endsWith("¦") || body[lineIndex].endsWith("•") ? "" : " ";
        const candidate = `${body[lineIndex]}${prefix}${separator}${word}`.trimEnd();
        if (codepoints(candidate) <= width) {
            body[lineIndex] = candidate;
            prefix = "";
            continue;
        }
        body.push(word);
        lineIndex = body.length - 1;
        prefix = "";
    }
    return placement;
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

    const body: string[] = [];
    const relativeLines = new Map<number, number>();
    for (const entry of entries) {
        const bodyLine = appendEntry(body, displayCue(entry), innerWidth);
        relativeLines.set(entry.id, bodyLine + 4);
    }
    for (const merge of merges) {
        const targetLine =
            merge.mergeInto === undefined ? undefined : relativeLines.get(merge.mergeInto);
        if (targetLine === undefined) throw new Error(`merge target line missing for ${merge.id}`);
        relativeLines.set(merge.id, targetLine);
    }

    const peakImportance = Math.max(...allEntries.map((entry) => entry.importance));
    const high = peakImportance >= 70;
    const [tl, fill, tr, side, middleLeft, middleRight, bl, br] = high
        ? ["╔", "═", "╗", "║", "╠", "╣", "╚", "╝"]
        : ["┌", "─", "┐", "│", "├", "┤", "└", "┘"];
    const titlePadding = innerWidth - codepoints(name);
    const title = `${" ".repeat(Math.floor(titlePadding / 2))}${name}${" ".repeat(Math.ceil(titlePadding / 2))}`;
    const lines = [
        `${tl}${fill.repeat(innerWidth)}${tr}`,
        `${side}${title}${side}`,
        `${side}${" ".repeat(innerWidth)}${side}`,
        `${middleLeft}${fill.repeat(innerWidth)}${middleRight}`,
        ...body.map((line) => `${side}${line.padEnd(innerWidth)}${side}`),
        `${bl}${fill.repeat(innerWidth)}${br}`,
    ];
    return { category, name, lines, relativeLines, entries, merges, peakImportance };
}

function renderPalace(specs: SpecEntry[]): {
    palace: string;
    placements: Map<number, Placement>;
    rooms: RoomSummary[];
    layoutItems: LayoutItem[];
} {
    const grouped = new Map<string, SpecEntry[]>();
    for (const spec of specs) {
        const key = `${spec.category}\u0000${spec.room}`;
        const list = grouped.get(key) ?? [];
        list.push(spec);
        grouped.set(key, list);
    }

    const columns = Array.from({ length: COLUMN_COUNT }, () => [] as string[]);
    const heights = Array<number>(COLUMN_COUNT).fill(0);
    const placements = new Map<number, Placement>();
    const roomSummaries: RoomSummary[] = [];
    const layoutItems: LayoutItem[] = [];

    const shortestColumn = (): number => {
        let selected = 0;
        for (let column = 1; column < COLUMN_COUNT; column++) {
            if (heights[column] < heights[selected]) selected = column;
        }
        return selected;
    };
    const placeLines = (lines: string[]): { column: number; row: number } => {
        const column = shortestColumn();
        const row = heights[column];
        columns[column].push(...lines);
        heights[column] += lines.length;
        return { column, row };
    };
    const categoryBanner = (category: Category): string => {
        const label = ` <${category}> `;
        const remaining = ROOM_WIDTH - codepoints(label);
        return `${"─".repeat(Math.floor(remaining / 2))}${label}${"─".repeat(Math.ceil(remaining / 2))}`;
    };

    for (const category of CATEGORY_ORDER) {
        const banner = placeLines([categoryBanner(category)]);
        layoutItems.push({
            kind: "category",
            category,
            column: banner.column,
            startLine: banner.row + 1,
            endLine: banner.row + 1,
        });

        const boxes = [...grouped.entries()]
            .filter(([key]) => key.startsWith(`${category}\u0000`))
            .map(([key, entries]) => buildBox(category, key.slice(category.length + 1), entries))
            .sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
        for (const box of boxes) {
            const placed = placeLines(box.lines);
            const startLine = placed.row + 1;
            for (const entry of [...box.entries, ...box.merges]) {
                const relativeLine = box.relativeLines.get(entry.id);
                if (relativeLine === undefined)
                    throw new Error(`placement missing for ${entry.id}`);
                placements.set(entry.id, {
                    category: box.category,
                    room: box.name,
                    palaceLine: startLine + relativeLine,
                    palaceColumn: placed.column * (ROOM_WIDTH + COLUMN_GAP) + 1,
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
                column: placed.column,
                startLine,
                endLine: startLine + box.lines.length - 1,
                heightCells: box.lines.length,
            });
            layoutItems.push({
                kind: "room",
                category: box.category,
                room: box.name,
                column: placed.column,
                startLine,
                endLine: startLine + box.lines.length - 1,
            });
        }
    }

    const canvasHeight = Math.max(...heights);
    const palaceLines = Array.from({ length: canvasHeight }, (_, row) =>
        columns
            .map((column) => (column[row] ?? "").padEnd(ROOM_WIDTH))
            .join(" ".repeat(COLUMN_GAP))
            .trimEnd(),
    );
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
    return { palace, placements, rooms: roomSummaries, layoutItems };
}

const sourceText = readFileSync(SOURCE_PATH, "utf8");
const source = parseSource(sourceText);
const specs = readSpecs();
validate(source, specs);
const { palace, placements, rooms, layoutItems } = renderPalace(specs);
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
        columns: COLUMN_COUNT,
        roomWidthChars: ROOM_WIDTH,
        columnGapChars: COLUMN_GAP,
        canvasHeightCells: palace.trimEnd().split("\n").length,
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
writeFileSync(join(HERE, "palace.txt"), palace);
writeFileSync(join(HERE, "coverage.json"), `${JSON.stringify(coverage, null, 4)}\n`);
console.log(
    JSON.stringify({
        palace: basename(join(HERE, "palace.txt")),
        chars: palace.length,
        lines: palace.trimEnd().split("\n").length,
        entries: entryCount,
        merges: mergeCount,
        memories: placements.size,
        rooms: rooms.length,
    }),
);
