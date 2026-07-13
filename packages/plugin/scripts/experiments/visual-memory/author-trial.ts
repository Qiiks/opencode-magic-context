#!/usr/bin/env bun
/**
 * Single-shot OpenRouter trial for memory-palace cue authoring. It deliberately
 * reuses author-palace.ts validation instead of applying model output to files.
 */
import { mkdirSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { isExactToken, validate } from "./author-palace.ts";

const HERE = dirname(fileURLToPath(import.meta.url));
const SOURCE_PATH = "/tmp/visual-memory/trimmed-memories-source.txt";
const OUTPUT_DIR = "/tmp/visual-memory";
const REPORT_PATH = join(HERE, "TRIAL-REPORT.md");
const SYSTEM_PROMPT_PATH = join(HERE, "author-trial-system-prompt.md");
const OPENROUTER_ENDPOINT = "https://openrouter.ai/api/v1/chat/completions";
const MAX_OUTPUT_TOKENS = 16_384;
const ANCHOR_FIDELITY_FLOOR = 85;

const CATEGORY_ORDER = [
    "PROJECT_RULES",
    "ARCHITECTURE",
    "CONSTRAINTS",
    "CONFIG_VALUES",
    "NAMING",
    "KNOWN_ISSUES",
] as const;
const TRIAL_CATEGORIES = ["PROJECT_RULES", "ARCHITECTURE"] as const;

type Category = (typeof CATEGORY_ORDER)[number];
type TrialCategory = (typeof TRIAL_CATEGORIES)[number];
type SpecEntry = {
    id: number;
    category: Category;
    room: string;
    cue?: string | string[];
    mergeInto?: number;
    importance: number;
};
type SourceMemory = {
    id: number;
    category: Category;
    text: string;
    importance: number;
};
type FailureKind =
    | "missing polarity mechanism"
    | "hub noun repetition"
    | "memory ID leakage"
    | "broken exact anchors"
    | "unbalanced parentheses"
    | "other validator failures";
type ValidationFailure = { id?: number; message: string };
type Assessment = {
    coverage: { covered: number; total: number; uncovered: number[] };
    manifestValidationError?: string;
    failures: Record<FailureKind, ValidationFailure[]>;
    anchorFidelity: { matched: number; total: number; percent: number };
    importance: { matched: number; total: number; mismatches: number[] };
    rooms: { generated: string[]; frontier: string[] };
    samples: Array<{ id: number; frontierCue: string; generatedCue: string }>;
};
type TrialResult = {
    label: string;
    model: string;
    category: TrialCategory;
    rawPath: string;
    attempts: number;
    retry: { attempted: boolean; recovered?: boolean; initialParseError?: string };
    requestError?: string;
    parseError?: string;
    assessment?: Assessment;
};

type ChatMessage = { role: "system" | "user"; content: string };

const FAILURE_KINDS: FailureKind[] = [
    "missing polarity mechanism",
    "hub noun repetition",
    "memory ID leakage",
    "broken exact anchors",
    "unbalanced parentheses",
    "other validator failures",
];

function errorMessage(error: unknown): string {
    return error instanceof Error ? error.message : String(error);
}

function parseSourceMemories(source: string, importanceById: Map<number, number>): SourceMemory[] {
    const memories: SourceMemory[] = [];
    let category: Category | undefined;
    let current: { id: number; category: Category; lines: string[] } | undefined;

    const flush = (): void => {
        if (!current) return;
        const importance = importanceById.get(current.id);
        if (importance === undefined) {
            throw new Error(`frontier spec has no importance for source memory ${current.id}`);
        }
        memories.push({
            id: current.id,
            category: current.category,
            text: current.lines.join("\n").trim(),
            importance,
        });
        current = undefined;
    };

    for (const line of source.split("\n")) {
        const open = line.match(/^<([A-Z_]+)>$/)?.[1];
        if (open) {
            flush();
            if (!CATEGORY_ORDER.includes(open as Category)) {
                throw new Error(`unknown source category ${open}`);
            }
            category = open as Category;
            continue;
        }
        if (/^<\/[A-Z_]+>$/.test(line)) {
            flush();
            category = undefined;
            continue;
        }
        const memory = line.match(/^#(\d+):\s?(.*)$/);
        if (memory) {
            flush();
            if (!category) throw new Error(`memory ${memory[1]} is outside a category`);
            current = {
                id: Number(memory[1]),
                category,
                lines: [memory[2] ?? ""],
            };
            continue;
        }
        if (current) current.lines.push(line);
    }
    flush();
    return memories;
}

function readFrontierSpecs(): SpecEntry[] {
    return readdirSync(HERE)
        .filter((file) => file.startsWith("spec-") && file.endsWith(".json"))
        .sort()
        .flatMap((file) => JSON.parse(readFileSync(join(HERE, file), "utf8")) as SpecEntry[]);
}

function renderCategoryPrompt(category: TrialCategory, memories: SourceMemory[]): string {
    const pool = memories
        .map(
            (memory) =>
                `#${memory.id} [importance=${memory.importance}]\n${memory.text}`,
        )
        .join("\n\n");
    return `# Palace cue authoring task\n\nAuthor the complete ${category} manifest below. The header ID and importance are required output fields; copy importance exactly. The source text is the only factual input.\n\n<${category}>\n${pool}\n</${category}>`;
}

function parseSpecArray(raw: string): { specs?: SpecEntry[]; error?: string } {
    let parsed: unknown;
    try {
        parsed = JSON.parse(raw);
    } catch (error) {
        return { error: `JSON parse failed: ${errorMessage(error)}` };
    }
    if (!Array.isArray(parsed)) return { error: "JSON root must be an array" };
    if (parsed.some((entry) => typeof entry !== "object" || entry === null || Array.isArray(entry))) {
        return { error: "JSON array must contain only object entries" };
    }
    return { specs: parsed as SpecEntry[] };
}

function responseContent(payload: unknown): string | undefined {
    if (typeof payload !== "object" || payload === null) return undefined;
    const choices = (payload as { choices?: unknown }).choices;
    if (!Array.isArray(choices)) return undefined;
    const first = choices[0];
    if (typeof first !== "object" || first === null) return undefined;
    const message = (first as { message?: unknown }).message;
    if (typeof message !== "object" || message === null) return undefined;
    const content = (message as { content?: unknown }).content;
    if (typeof content === "string") return content;
    if (typeof content === "object" && content !== null && !Array.isArray(content)) {
        const value = content as { text?: unknown; content?: unknown; value?: unknown };
        for (const candidate of [value.text, value.content, value.value]) {
            if (typeof candidate === "string") return candidate;
        }
        return undefined;
    }
    if (!Array.isArray(content)) return undefined;
    const parts = content.flatMap((part) => {
        if (typeof part !== "object" || part === null) return [];
        const value = part as { text?: unknown; content?: unknown; value?: unknown };
        const text = typeof value.text === "string" ? value.text : value.content ?? value.value;
        return typeof text === "string" ? [text] : [];
    });
    return parts.length > 0 ? parts.join("") : undefined;
}

function responseShape(payload: unknown): string {
    if (typeof payload !== "object" || payload === null) return "non-object payload";
    const choices = (payload as { choices?: unknown }).choices;
    if (!Array.isArray(choices)) return `top-level keys: ${Object.keys(payload).join(", ")}`;
    const first = choices[0];
    if (typeof first !== "object" || first === null) return `choices=${choices.length}`;
    const message = (first as { message?: unknown }).message;
    const finishReason = (first as { finish_reason?: unknown }).finish_reason;
    if (typeof message !== "object" || message === null) {
        return `choices=${choices.length}; finish_reason=${String(finishReason)}; no message object`;
    }
    const content = (message as { content?: unknown }).content;
    const contentShape =
        typeof content === "object" && content !== null && !Array.isArray(content)
            ? `object keys: ${Object.keys(content).join(", ")}`
            : Array.isArray(content)
              ? "array"
              : typeof content;
    return `choices=${choices.length}; finish_reason=${String(finishReason)}; message keys: ${Object.keys(message).join(", ")}; content type: ${contentShape}`;
}

async function callOpenRouter(
    apiKey: string,
    model: string,
    messages: ChatMessage[],
): Promise<string> {
    const response = await fetch(OPENROUTER_ENDPOINT, {
        method: "POST",
        headers: {
            Authorization: `Bearer ${apiKey}`,
            "Content-Type": "application/json",
        },
        body: JSON.stringify({
            model,
            messages,
            temperature: 0.1,
            max_tokens: MAX_OUTPUT_TOKENS,
            reasoning: { enabled: false },
        }),
        signal: AbortSignal.timeout(10 * 60 * 1_000),
    });
    const responseText = await response.text();
    if (!response.ok) {
        const summary = responseText.replace(/\s+/g, " ").slice(0, 500);
        throw new Error(`OpenRouter ${response.status}: ${summary}`);
    }
    let payload: unknown;
    try {
        payload = JSON.parse(responseText);
    } catch (error) {
        throw new Error(`OpenRouter returned non-JSON: ${errorMessage(error)}`);
    }
    const content = responseContent(payload);
    if (!content) throw new Error(`OpenRouter response has no assistant text (${responseShape(payload)})`);
    return content;
}

function rawOutputPath(label: string, category: TrialCategory): string {
    return join(OUTPUT_DIR, `trial-${label}-${category}.json`);
}

async function runTrial(args: {
    apiKey: string;
    label: string;
    model: string;
    category: TrialCategory;
    source: SourceMemory[];
    allSource: SourceMemory[];
    frontier: SpecEntry[];
    systemPrompt: string;
}): Promise<TrialResult> {
    const categorySource = args.source.filter((memory) => memory.category === args.category);
    const prompt = renderCategoryPrompt(args.category, categorySource);
    const rawPath = rawOutputPath(args.label, args.category);
    const baseMessages: ChatMessage[] = [
        { role: "system", content: args.systemPrompt },
        { role: "user", content: prompt },
    ];
    let raw: string;
    try {
        raw = await callOpenRouter(args.apiKey, args.model, baseMessages);
    } catch (error) {
        return {
            label: args.label,
            model: args.model,
            category: args.category,
            rawPath,
            attempts: 1,
            retry: { attempted: false },
            requestError: errorMessage(error),
        };
    }

    let parsed = parseSpecArray(raw);
    let attempts = 1;
    const retry: TrialResult["retry"] = { attempted: false };
    if (!parsed.specs) {
        retry.attempted = true;
        retry.initialParseError = parsed.error;
        writeFileSync(rawPath.replace(/\.json$/, ".attempt-1.raw"), raw);
        attempts++;
        try {
            raw = await callOpenRouter(args.apiKey, args.model, [
                ...baseMessages,
                {
                    role: "user",
                    content: `The previous response was rejected before validation: ${parsed.error ?? "invalid JSON"}. Return a fresh, complete JSON array only, including its final ].`,
                },
            ]);
        } catch (error) {
            return {
                label: args.label,
                model: args.model,
                category: args.category,
                rawPath,
                attempts,
                retry,
                requestError: errorMessage(error),
            };
        }
        parsed = parseSpecArray(raw);
        retry.recovered = Boolean(parsed.specs);
    }

    writeFileSync(rawPath, raw);
    if (!parsed.specs) {
        return {
            label: args.label,
            model: args.model,
            category: args.category,
            rawPath,
            attempts,
            retry,
            parseError: parsed.error,
        };
    }

    return {
        label: args.label,
        model: args.model,
        category: args.category,
        rawPath,
        attempts,
        retry,
        assessment: assessCandidate({
            category: args.category,
            source: categorySource,
            allSource: args.allSource,
            frontier: args.frontier,
            generated: parsed.specs,
        }),
    };
}

function emptyFailures(): Record<FailureKind, ValidationFailure[]> {
    return Object.fromEntries(FAILURE_KINDS.map((kind) => [kind, []])) as Record<
        FailureKind,
        ValidationFailure[]
    >;
}

function classifyValidatorError(message: string): FailureKind {
    if (
        /negative rule missing polarity marker|polarity mechanism missing|polarity mechanism must follow marker/i.test(
            message,
        )
    ) {
        return "missing polarity mechanism";
    }
    if (/hub noun repeated in cue/i.test(message)) return "hub noun repetition";
    if (/memory id leaked into cue/i.test(message)) return "memory ID leakage";
    if (/exact anchor .* missing from rendered cue/i.test(message)) return "broken exact anchors";
    if (/polarity mechanism is unclosed|unbalanced mechanism/i.test(message)) {
        return "unbalanced parentheses";
    }
    return "other validator failures";
}

function validatorErrorId(message: string): number | undefined {
    const match = message.match(/\b(?:cue|spec id|merge)\s+(\d+)\b/i);
    return match ? Number(match[1]) : undefined;
}

function addFailure(
    failures: Record<FailureKind, ValidationFailure[]>,
    seen: Set<string>,
    message: string,
    id?: number,
): void {
    const kind = classifyValidatorError(message);
    const key = `${kind}\u0000${id ?? ""}\u0000${message}`;
    if (seen.has(key)) return;
    seen.add(key);
    failures[kind].push({ ...(id === undefined ? {} : { id }), message });
}

function exactTokens(value: string): string[] {
    return [
        ...new Set(
            (value.match(/`[^`]+`|[^\s()`]+/g) ?? [])
                .filter((token) => token.startsWith("`") || isExactToken(token))
                .map((token) => token.replace(/^[,;]+|[,;]+$/g, ""))
                .filter(Boolean),
        ),
    ];
}

function cueText(entry: SpecEntry | undefined): string {
    if (!entry) return "";
    if (typeof entry.cue === "string") return entry.cue;
    if (Array.isArray(entry.cue)) return entry.cue.join("; ");
    return "";
}

function effectiveCue(id: number, entriesById: Map<number, SpecEntry>): string {
    const visited = new Set<number>();
    let entry = entriesById.get(id);
    while (entry) {
        if (visited.has(entry.id)) return "";
        visited.add(entry.id);
        if (entry.mergeInto === undefined) return cueText(entry);
        entry = entriesById.get(entry.mergeInto);
    }
    return "";
}

function cueField(entry: SpecEntry | undefined): string {
    if (!entry) return "MISSING";
    if (entry.mergeInto !== undefined) return JSON.stringify({ mergeInto: entry.mergeInto });
    return JSON.stringify(entry.cue);
}

function evenlySpaced<T>(values: T[], count: number): T[] {
    if (values.length <= count) return values;
    return Array.from({ length: count }, (_, index) => {
        const offset = Math.round((index * (values.length - 1)) / (count - 1));
        return values[offset] as T;
    });
}

function assessCandidate(args: {
    category: TrialCategory;
    source: SourceMemory[];
    allSource: SourceMemory[];
    frontier: SpecEntry[];
    generated: SpecEntry[];
}): Assessment {
    const failures = emptyFailures();
    const failureKeys = new Set<string>();
    const generatedIds = new Set<unknown>(args.generated.map((entry) => entry.id));
    const uncovered = args.source.filter((memory) => !generatedIds.has(memory.id)).map((memory) => memory.id);
    const generatedById = new Map<number, SpecEntry>();
    const occurrences = new Map<number, number>();

    for (const entry of args.generated) {
        if (typeof entry.id !== "number") {
            addFailure(failures, failureKeys, `spec id is not numeric: ${String(entry.id)}`);
            continue;
        }
        generatedById.set(entry.id, entry);
        occurrences.set(entry.id, (occurrences.get(entry.id) ?? 0) + 1);
    }
    for (const [id, count] of occurrences) {
        if (count > 1) {
            addFailure(failures, failureKeys, `duplicate spec id ${id} (${count} entries)`, id);
        }
    }

    const sourceById = new Map(args.allSource.map((memory) => [memory.id, memory]));
    for (const entry of args.generated) {
        if (typeof entry.id !== "number") continue;
        const sourceMemory = sourceById.get(entry.id);
        if (!sourceMemory) {
            addFailure(failures, failureKeys, `spec id ${entry.id} absent from source`, entry.id);
            continue;
        }
        const replacements = new Map<number, SpecEntry>([[entry.id, entry]]);
        let changed = true;
        while (changed) {
            changed = false;
            for (const candidate of args.generated) {
                if (
                    typeof candidate.id === "number" &&
                    candidate.mergeInto !== undefined &&
                    replacements.has(candidate.mergeInto) &&
                    !replacements.has(candidate.id)
                ) {
                    replacements.set(candidate.id, candidate);
                    changed = true;
                }
            }
            for (const frontierEntry of args.frontier) {
                if (
                    frontierEntry.mergeInto !== undefined &&
                    replacements.has(frontierEntry.mergeInto) &&
                    !replacements.has(frontierEntry.id)
                ) {
                    const generatedReplacement = generatedById.get(frontierEntry.id);
                    if (generatedReplacement) {
                        replacements.set(generatedReplacement.id, generatedReplacement);
                        changed = true;
                    }
                }
            }
            for (const candidate of [...replacements.values()]) {
                if (candidate.mergeInto === undefined || replacements.has(candidate.mergeInto)) continue;
                const target = generatedById.get(candidate.mergeInto);
                if (target) {
                    replacements.set(target.id, target);
                    changed = true;
                }
            }
        }
        const overlay = args.frontier.map(
            (frontierEntry) => replacements.get(frontierEntry.id) ?? frontierEntry,
        );
        try {
            validate(args.allSource, overlay);
        } catch (error) {
            const message = errorMessage(error);
            addFailure(failures, failureKeys, message, validatorErrorId(message) ?? entry.id);
        }
    }

    let manifestValidationError: string | undefined;
    try {
        validate(
            args.allSource,
            [
                ...args.frontier.filter((entry) => entry.category !== args.category),
                ...args.generated,
            ],
        );
    } catch (error) {
        manifestValidationError = errorMessage(error);
        addFailure(failures, failureKeys, manifestValidationError, validatorErrorId(manifestValidationError));
    }

    let matchedAnchors = 0;
    let totalAnchors = 0;
    for (const memory of args.source) {
        const sourceTokens = exactTokens(memory.text);
        const cue = effectiveCue(memory.id, generatedById);
        totalAnchors += sourceTokens.length;
        matchedAnchors += sourceTokens.filter((token) => cue.includes(token)).length;
    }

    const importanceMismatches: number[] = [];
    for (const memory of args.source) {
        const entry = generatedById.get(memory.id);
        if (!entry || entry.importance !== memory.importance) importanceMismatches.push(memory.id);
    }

    const frontierById = new Map(args.frontier.map((entry) => [entry.id, entry]));
    const samples = evenlySpaced(args.source, 6).map((memory) => ({
        id: memory.id,
        frontierCue: cueField(frontierById.get(memory.id)),
        generatedCue: cueField(generatedById.get(memory.id)),
    }));
    const generatedRooms = [
        ...new Set(
            args.generated
                .filter((entry) => typeof entry.room === "string")
                .map((entry) => entry.room)
                .sort((a, b) => a.localeCompare(b)),
        ),
    ];
    const frontierRooms = [
        ...new Set(
            args.frontier
                .filter((entry) => entry.category === args.category)
                .map((entry) => entry.room)
                .sort((a, b) => a.localeCompare(b)),
        ),
    ];

    return {
        coverage: {
            covered: args.source.length - uncovered.length,
            total: args.source.length,
            uncovered,
        },
        ...(manifestValidationError ? { manifestValidationError } : {}),
        failures,
        anchorFidelity: {
            matched: matchedAnchors,
            total: totalAnchors,
            percent: totalAnchors === 0 ? 100 : Number(((matchedAnchors / totalAnchors) * 100).toFixed(1)),
        },
        importance: {
            matched: args.source.length - importanceMismatches.length,
            total: args.source.length,
            mismatches: importanceMismatches,
        },
        rooms: { generated: generatedRooms, frontier: frontierRooms },
        samples,
    };
}

function inline(value: string): string {
    return value.replace(/`/g, "\\`").replace(/\n/g, " ");
}

function renderFailures(failures: Record<FailureKind, ValidationFailure[]>): string[] {
    const lines: string[] = [];
    for (const kind of FAILURE_KINDS) {
        const entries = failures[kind];
        lines.push(`- **${kind}:** ${entries.length}`);
        for (const entry of entries.slice(0, 3)) {
            lines.push(
                `  - ${entry.id === undefined ? "manifest" : `#${entry.id}`}: ${inline(entry.message)}`,
            );
        }
    }
    return lines;
}

function renderAssessment(result: TrialResult): string[] {
    const lines = [
        `## ${result.label}: ${result.category}`,
        "",
        `- Model: \`${result.model}\``,
        `- Raw completion: \`${result.rawPath}\``,
        `- Calls: ${result.attempts}; parse retry: ${
            result.retry.attempted
                ? result.retry.recovered
                    ? "recovered"
                    : "attempted but did not recover"
                : "not needed"
        }`,
    ];
    if (result.retry.initialParseError) {
        lines.push(`- First parse rejection: ${inline(result.retry.initialParseError)}`);
    }
    if (result.requestError) {
        lines.push(
            `- Request failure: ${inline(result.requestError)}`,
            "- Coverage, validator failures, anchor fidelity, room quality, and side-by-side cues: not measured because no completion was available.",
            "",
        );
        return lines;
    }
    if (result.parseError) {
        lines.push(
            `- Fail-closed parse rejection: ${inline(result.parseError)}`,
            "- Coverage: not measured because the complete JSON root was rejected.",
            "- Hard validator failures: not measured because validation never receives a partial manifest.",
            "- Anchor fidelity: not measured because validation never receives a partial manifest.",
            "- Room quality and side-by-side cues: not measured because validation never receives a partial manifest.",
            "",
        );
        return lines;
    }

    const assessment = result.assessment;
    if (!assessment) return [...lines, "- No assessment was produced.", ""];
    lines.push(
        `- Coverage: **${assessment.coverage.covered}/${assessment.coverage.total}**; uncovered: ${
            assessment.coverage.uncovered.length > 0 ? assessment.coverage.uncovered.join(", ") : "none"
        }`,
        `- Anchor fidelity: **${assessment.anchorFidelity.percent}%** (${assessment.anchorFidelity.matched}/${assessment.anchorFidelity.total} source exact tokens retained in the effective cue)`,
        `- Importance passthrough: **${assessment.importance.matched}/${assessment.importance.total}**; mismatches: ${
            assessment.importance.mismatches.length > 0 ? assessment.importance.mismatches.join(", ") : "none"
        }`,
        `- Full-manifest validator: ${
            assessment.manifestValidationError ? `failed — ${inline(assessment.manifestValidationError)}` : "passed"
        }`,
        "",
        "### Hard validator failures",
        "",
        ...renderFailures(assessment.failures),
        "",
        "### Room quality",
        "",
        `- ${result.label} rooms (${assessment.rooms.generated.length}): ${assessment.rooms.generated.join(", ") || "none"}`,
        `- Frontier rooms (${assessment.rooms.frontier.length}): ${assessment.rooms.frontier.join(", ") || "none"}`,
        "",
        "### Six evenly spaced cue comparisons",
        "",
    );
    for (const sample of assessment.samples) {
        lines.push(
            `#### #${sample.id}`,
            "",
            "Frontier cue:",
            "```json",
            sample.frontierCue,
            "```",
            "",
            `${result.label} cue:`,
            "```json",
            sample.generatedCue,
            "```",
            "",
        );
    }
    return lines;
}

function qualityGaps(result: TrialResult): string[] {
    if (result.requestError) return ["request failure"];
    if (result.parseError) return ["fail-closed parse rejection"];
    const assessment = result.assessment;
    if (!assessment) return ["missing assessment"];
    const gaps: string[] = [];
    if (assessment.coverage.uncovered.length > 0) gaps.push("uncovered memories");
    const failedKinds = FAILURE_KINDS.filter((kind) => assessment.failures[kind].length > 0);
    gaps.push(...failedKinds);
    if (assessment.anchorFidelity.percent < ANCHOR_FIDELITY_FLOOR) {
        gaps.push(`anchor fidelity ${assessment.anchorFidelity.percent}% < ${ANCHOR_FIDELITY_FLOOR}%`);
    }
    if (assessment.importance.mismatches.length > 0) gaps.push("importance passthrough mismatches");
    return [...new Set(gaps)];
}

function verdict(ds4f: TrialResult[], baseline: TrialResult | undefined): string {
    const gaps = ds4f.flatMap((result) =>
        qualityGaps(result).map((gap) => `${result.category}: ${gap}`),
    );
    const baselineSignal = !baseline
        ? "No GPT-5.6-sol baseline was requested."
        : baseline.requestError
          ? "The GPT-5.6-sol sanity baseline was unavailable through this OpenRouter key."
          : baseline.parseError
            ? "The GPT-5.6-sol sanity baseline also failed fail-closed parsing, so inspect the harness before drawing model conclusions."
            : "The GPT-5.6-sol sanity baseline completed and is included above for comparison.";
    if (gaps.length === 0) {
        return `**SHIP-ON-FLASH.** Both DS4F categories cleared the acceptance gate: complete fail-closed JSON, full coverage, no validator-class failures, exact importance passthrough, and at least ${ANCHOR_FIDELITY_FLOOR}% anchor fidelity. ${baselineSignal}`;
    }
    return `**SHIP-WITH-STRONGER-MODEL-RECOMMENDED.** DS4F missed the acceptance gate because ${gaps.join("; ")}. ${baselineSignal}`;
}

function requestedCategories(): TrialCategory[] {
    const args = process.argv.slice(2);
    if (args.length === 0) return [...TRIAL_CATEGORIES];
    const categories = args.map((arg) => arg.toUpperCase());
    const invalid = categories.filter(
        (category) => !TRIAL_CATEGORIES.includes(category as TrialCategory),
    );
    if (invalid.length > 0) {
        throw new Error(`use only PROJECT_RULES or ARCHITECTURE; received ${invalid.join(", ")}`);
    }
    return categories as TrialCategory[];
}

async function main(): Promise<void> {
    mkdirSync(OUTPUT_DIR, { recursive: true });
    const apiKey = readFileSync(join(homedir(), ".config", "openrouter.key"), "utf8").trim();
    if (!apiKey) throw new Error("~/.config/openrouter.key is empty");
    const systemPrompt = readFileSync(SYSTEM_PROMPT_PATH, "utf8").trim();
    const frontier = readFrontierSpecs();
    const importanceById = new Map(frontier.map((entry) => [entry.id, entry.importance]));
    const source = parseSourceMemories(readFileSync(SOURCE_PATH, "utf8"), importanceById);
    const categories = requestedCategories();

    const ds4f: TrialResult[] = [];
    for (const category of categories) {
        ds4f.push(
            await runTrial({
                apiKey,
                label: "ds4f",
                model: "deepseek/deepseek-v4-flash",
                category,
                source,
                allSource: source,
                frontier,
                systemPrompt,
            }),
        );
    }

    let baseline: TrialResult | undefined;
    if (categories.includes("PROJECT_RULES")) {
        baseline = await runTrial({
            apiKey,
            label: "gpt-5.6-sol",
            model: "openai/gpt-5.6-sol",
            category: "PROJECT_RULES",
            source,
            allSource: source,
            frontier,
            systemPrompt,
        });
    }

    const report = [
        "# Palace Authoring Trial",
        "",
        `Run: ${new Date().toISOString()}`,
        "",
        "This is a non-agentic, single-call-per-category authoring trial. JSON parsing is fail-closed: no fence stripping, substring extraction, or partial manifest application. Each generated entry is overlaid onto the frontier manifest and checked with the exported `author-palace.ts` validator; the full generated category is also checked in one combined manifest. The acceptance gate requires complete JSON, 100% coverage, no validator-class failures, exact importance passthrough, and at least 85% exact-anchor fidelity.",
        "",
        "# DS4F results",
        "",
        ...ds4f.flatMap(renderAssessment),
        "# GPT-5.6-sol sanity baseline",
        "",
        ...(baseline ? renderAssessment(baseline) : ["Not run: PROJECT_RULES was not selected.", ""]),
        "# Verdict",
        "",
        verdict(ds4f, baseline),
        "",
    ].join("\n");
    writeFileSync(REPORT_PATH, report);
    console.log(`report=${REPORT_PATH}`);
    for (const result of ds4f) {
        const coverage = result.assessment?.coverage;
        console.log(
            `${result.label}/${result.category}: ${
                result.requestError ?? result.parseError ?? `${coverage?.covered}/${coverage?.total} covered`
            }`,
        );
    }
    if (baseline) {
        const coverage = baseline.assessment?.coverage;
        console.log(
            `${baseline.label}/${baseline.category}: ${
                baseline.requestError ?? baseline.parseError ?? `${coverage?.covered}/${coverage?.total} covered`
            }`,
        );
    }
}

void main().catch((error) => {
    console.error(`author-trial failed: ${errorMessage(error)}`);
    process.exitCode = 1;
});
