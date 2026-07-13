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
const MAX_LINE_CHARS = 152;
const MAX_PALACE_CHARS = 20_000;
const TARGET_CUE_CHARS = 8;
const GRID_COLUMNS = 4;
const GRID_COLUMN_WIDTH = 37;
const GRID_GAP = 0;
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
type Placement = { category: Category; room: string; palaceLine: number; mergedInto?: number };
type RoomSummary = {
    category: Category;
    name: string;
    entryCount: number;
    mergeCount: number;
    memoryCount: number;
    peakImportance: number;
    border: "single" | "double";
    startLine: number;
    endLine: number;
};

type Box = {
    category: Category;
    name: string;
    width: number;
    span: number;
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
    if (hubWords.length > 0)
        value = value.replace(new RegExp(`\\b(?:${hubWords.join("|")})\\b`, "gi"), "");
    const replacements: Array<[RegExp, string]> = [
        [/\bconfigurations?\b/gi, "cfg"],
        [/\bbackground\b/gi, "bg"],
        [/\bprojects?\b/gi, "proj"],
        [/\bworktrees?\b/gi, "wt"],
        [/\bplugins?\b/gi, "plug"],
        [/\bmemories?\b/gi, "mem"],
        [/\bsemantic\b/gi, "sem"],
        [/\bprocess(?:es)?\b/gi, "proc"],
        [/\bsessions?\b/gi, "sess"],
        [/\bstorage\b/gi, "store"],
        [/\breleases?\b/gi, "rel"],
        [/\bpackages?\b/gi, "pkg"],
        [/\bresponses?\b/gi, "reply"],
        [/\bresults?\b/gi, "out"],
        [/\benvironment\b/gi, "env"],
        [/\bidentit(?:y|ies)\b/gi, "ID"],
        [/\bperformance\b/gi, "perf"],
        [/\bimportance\b/gi, "imp"],
        [/\btemporary\b/gi, "tmp"],
        [/\boperation(?:s)?\b/gi, "op"],
        [/\bparameters?\b/gi, "params"],
        [/\barguments?\b/gi, "args"],
        [/\bnotifications?\b/gi, "notif"],
        [/\bcompletion(?:s)?\b/gi, "done"],
        [/\bverification\b/gi, "verify"],
        [/\bdeterministic\b/gi, "stable"],
        [/\btransaction(?:al)?\b/gi, "txn"],
        [/\bdiagnostics\b/gi, "diags"],
        [/\bsynchronous\b/gi, "sync"],
        [/\basynchronous\b/gi, "async"],
        [/\bconcurrent(?:ly|cy)?\b/gi, "parallel"],
        [/\boptional\b/gi, "opt"],
        [/\bminimum\b/gi, "min"],
        [/\bmaximum\b/gi, "max"],
        [/\btemporary\b/gi, "temp"],
        [/\bdifferent\b/gi, "≠"],
        [/\bsame\b/gi, "="],
        [/\bempty\b/gi, "∅"],
        [/\bmissing\b/gi, "∅"],
        [/\breturns?\b/gi, "→"],
        [/\bwrites?\b/gi, "→"],
        [/\breads?\b/gi, "←"],
        [/\bsets?\b/gi, "="],
        [/\bincludes?\b/gi, "+"],
        [/\bcontains?\b/gi, "has"],
        [/\bsupports?\b/gi, "+"],
        [/\bwithout\b/gi, "⊘"],
        [/\binstead of\b/gi, "⊘"],
        [/\brather than\b/gi, "⊘"],
        [/\bto prevent\b/gi, "⊘"],
        [/\bprevent(?:s|ing)?\b/gi, "⊘"],
        [/\bbefore\b/gi, "≺"],
        [/\bafter\b/gi, "≻"],
        [/\bbecause\b/gi, "∵"],
        [/\brequires?\b/gi, "→"],
        [/\busing\b/gi, "via"],
        [/\bwith\b/gi, "+"],
        [/\band\b/gi, "+"],
        [/\bor\b/gi, "|"],
        [/\bevery\b/gi, "∀"],
        [/\bnone\b/gi, "∅"],
        [/\ball\b/gi, "∀"],
        [/\bzero\b/gi, "0"],
        [/\bsource\b/gi, "src"],
        [/\bfunction\b/gi, "fn"],
        [/\bdefault\b/gi, "dflt"],
        [/\bcurrent\b/gi, "cur"],
        [/\bexisting\b/gi, "existing"],
        [/\bthe\b/gi, ""],
        [/\ban?\b/gi, ""],
        [/\s*;\s*/g, ";"],
        [/\s*\+\s*/g, "+"],
        [/\s*→\s*/g, "→"],
        [/\s*←\s*/g, "←"],
        [/\s*=\s*/g, "="],
        [/\s*\|\s*/g, "|"],
        [/\s*:\s*/g, ":"],
        [/\s*,\s*/g, ","],
        [/\s{2,}/g, " "],
    ];
    for (const [pattern, replacement] of replacements) value = value.replace(pattern, replacement);
    value = value.replace(/\b[a-z]{3,}\b/g, (word) => {
        if (/^qz\d+zq$/i.test(word)) return word;
        return `${word[0]}${word.slice(1).replace(/[aeiou]/g, "")}`;
    });
    value = protectedValues.reduce(
        (result, item, index) => result.replace(`QZ${index}ZQ`, () => item),
        value,
    );
    return value.trim().replace(/^[-:;,]+|[-:;,]+$/g, "");
}

function pruneCue(value: string): string {
    if (codepoints(value) <= TARGET_CUE_CHARS) return value;
    const words =
        value.match(
            /[^\s`]+`[^`]*`[^\s`]*|`[^`]*`|\([^)]*\)|[^\s`]*[\\/_:@$%<>=.|][^\s`]*|[^\s`()]+/g,
        ) ?? [];
    const keep = new Set<number>();
    const keepWindow = (index: number, before: number, after: number): void => {
        for (let offset = -before; offset <= after; offset++) {
            if (index + offset >= 0 && index + offset < words.length) keep.add(index + offset);
        }
    };
    words.forEach((word, index) => {
        const codeLike =
            /`[^`]+`/.test(word) ||
            isExactToken(word) ||
            (value.includes("⊘") && /^\([^()]+\)$/.test(word));
        if (codeLike) keepWindow(index, 0, 0);
        if (/[⊘←→≺≻]/.test(word)) keepWindow(index, 0, 1);
    });
    if (keep.size === 0) keep.add(0);
    let length = [...keep].reduce((total, index) => total + codepoints(words[index] ?? "") + 1, 0);
    for (let index = 0; index < words.length && length < TARGET_CUE_CHARS; index++) {
        if (keep.has(index)) continue;
        const next = codepoints(words[index] ?? "") + 1;
        if (length + next <= TARGET_CUE_CHARS) {
            keep.add(index);
            length += next;
        }
    }
    const pruned = words.filter((_, index) => keep.has(index)).join(" ");
    return pruned;
}

function displayCue(entry: SpecEntry): string {
    const raw = Array.isArray(entry.cue) ? entry.cue.join("; ") : (entry.cue ?? "");
    return pruneCue(compactCue(raw, entry.room));
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
    const peakImportance = Math.max(...allEntries.map((entry) => entry.importance));
    const requiredOuterWidth = Math.max(longestToken(entries) + 4, codepoints(name) + 4);
    const span = Math.ceil((requiredOuterWidth + GRID_GAP) / (GRID_COLUMN_WIDTH + GRID_GAP));
    if (span > GRID_COLUMNS) throw new Error(`room ${name} needs ${requiredOuterWidth} columns`);
    const width = span * GRID_COLUMN_WIDTH + (span - 1) * GRID_GAP;
    const innerWidth = width - 2;
    const body: string[] = [];
    const relativeLines = new Map<number, number>();
    for (const entry of entries) {
        const bodyLine = appendEntry(body, displayCue(entry), innerWidth);
        relativeLines.set(entry.id, bodyLine + 1);
    }
    for (const merge of merges) {
        const targetLine =
            merge.mergeInto === undefined ? undefined : relativeLines.get(merge.mergeInto);
        if (targetLine === undefined) throw new Error(`merge target line missing for ${merge.id}`);
        relativeLines.set(merge.id, targetLine);
    }

    const high = peakImportance >= 70;
    const [tl, fill, tr, side, bl, br] = high
        ? ["╔", "═", "╗", "║", "╚", "╝"]
        : ["┌", "─", "┐", "│", "└", "┘"];
    const title = ` ${name} `;
    const titleFill = Math.max(0, innerWidth - codepoints(title) - 1);
    const lines = [
        `${tl}${fill}${title}${fill.repeat(titleFill)}${tr}`,
        ...body.map((line) => `${side}${line.padEnd(innerWidth)}${side}`),
        `${bl}${fill.repeat(innerWidth)}${br}`,
    ];
    return { category, name, width, span, lines, relativeLines, entries, merges, peakImportance };
}

function renderPalace(specs: SpecEntry[]): {
    palace: string;
    placements: Map<number, Placement>;
    rooms: RoomSummary[];
} {
    const grouped = new Map<string, SpecEntry[]>();
    for (const spec of specs) {
        const key = `${spec.category}\u0000${spec.room}`;
        const list = grouped.get(key) ?? [];
        list.push(spec);
        grouped.set(key, list);
    }
    const placements = new Map<number, Placement>();
    const roomSummaries: RoomSummary[] = [];
    const palaceLines: string[] = [];
    for (const category of CATEGORY_ORDER) {
        palaceLines.push(`<${category}>`);
        const boxes = [...grouped.entries()]
            .filter(([key]) => key.startsWith(`${category}\u0000`))
            .map(([key, entries]) => buildBox(category, key.slice(category.length + 1), entries))
            .sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
        const heights = Array<number>(GRID_COLUMNS).fill(0);
        const placed: Array<{ box: Box; column: number; row: number; sharedTop: boolean }> = [];
        const footprintBottom = new Map<string, number>();
        for (const box of boxes) {
            let bestColumn = 0;
            let bestRow = Number.POSITIVE_INFINITY;
            for (let column = 0; column <= GRID_COLUMNS - box.span; column++) {
                const columnHeight = Math.max(...heights.slice(column, column + box.span));
                const footprint = `${column}:${box.span}`;
                const canShare = footprintBottom.get(footprint) === columnHeight;
                const row = canShare ? columnHeight - 1 : columnHeight;
                if (row < bestRow) {
                    bestColumn = column;
                    bestRow = row;
                }
            }
            const footprint = `${bestColumn}:${box.span}`;
            const sharedTop = footprintBottom.get(footprint) === bestRow + 1;
            placed.push({ box, column: bestColumn, row: bestRow, sharedTop });
            const bottom = bestRow + box.lines.length;
            footprintBottom.set(footprint, bottom);
            for (let column = bestColumn; column < bestColumn + box.span; column++)
                heights[column] = bottom;
        }

        const canvasHeight = Math.max(...heights);
        const canvas = Array.from({ length: canvasHeight }, () =>
            Array<string>(MAX_LINE_CHARS).fill(" "),
        );
        for (const { box, column, row, sharedTop } of placed) {
            const left = column * (GRID_COLUMN_WIDTH + GRID_GAP);
            box.lines.forEach((line, lineOffset) => {
                [...line].forEach((character, characterOffset) => {
                    const x = left + characterOffset;
                    const canvasLine = canvas[row + lineOffset];
                    if (!canvasLine) throw new Error(`canvas row missing for ${box.name}`);
                    if ((!sharedTop || lineOffset > 0) && canvasLine[x] !== " ") {
                        throw new Error(
                            `room overlap at ${category}:${box.name}:${row + lineOffset + 1}:${x + 1}`,
                        );
                    }
                    canvasLine[x] = character;
                });
            });
        }
        const canvasStart = palaceLines.length + 1;
        palaceLines.push(...canvas.map((line) => line.join("").trimEnd()));

        for (const { box, row } of placed) {
            const startLine = canvasStart + row;
            for (const entry of [...box.entries, ...box.merges]) {
                const relativeLine = box.relativeLines.get(entry.id);
                if (relativeLine === undefined)
                    throw new Error(`placement missing for ${entry.id}`);
                placements.set(entry.id, {
                    category: box.category,
                    room: box.name,
                    palaceLine: startLine + relativeLine,
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
                startLine,
                endLine: startLine + box.lines.length - 1,
            });
        }
    }

    const palace = `${palaceLines.join("\n")}\n`;
    const longLines = palaceLines
        .map((line, index) => ({ line: index + 1, chars: codepoints(line) }))
        .filter((item) => item.chars > MAX_LINE_CHARS);
    if (longLines.length > 0)
        throw new Error(`lines exceed ${MAX_LINE_CHARS}: ${JSON.stringify(longLines)}`);
    if (palace.length > MAX_PALACE_CHARS) {
        throw new Error(`palace has ${palace.length} chars (max ${MAX_PALACE_CHARS})`);
    }
    if (/#\d+/.test(palace)) throw new Error("memory id leaked into palace.txt");
    return { palace, placements, rooms: roomSummaries };
}

const sourceText = readFileSync(SOURCE_PATH, "utf8");
const source = parseSource(sourceText);
const specs = readSpecs();
validate(source, specs);
const { palace, placements, rooms } = renderPalace(specs);
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
