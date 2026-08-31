import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import type { MockResponse } from "./mock-provider/server";
import { RustTestHarness } from "./rust-harness";

export type ReplayBlockShape = {
    type: "text" | "thinking" | "redacted_thinking";
    text_bytes?: number;
    thinking_bytes?: number;
    signature_bytes?: number;
    dropped_tag?: number;
    empty?: boolean;
};

export type ReplayPassShape = {
    label: string;
    input_text_bytes: number;
    response: {
        blocks: ReplayBlockShape[];
        input_tokens: number;
        output_tokens: number;
    };
};

export type ReplayFixture = {
    schema: 1;
    source: { report: string; capture: string; sanitization: string };
    passes: ReplayPassShape[];
    adjudications?: Array<{
        pass: string;
        axis: "reasoning_signature_shapes";
        ts_only: string[];
        rust_only: string[];
        decision: "intentional_difference";
        reason: string;
        source: string;
    }>;
};

type ValueSpaceAxis = {
    classification: "matched_value_space" | "divergent_value_space";
    ts_only: string[];
    rust_only: string[];
    shared: string[];
    adjudication?: {
        decision: "intentional_difference";
        reason: string;
        source: string;
    };
};

export type ReplayDivergenceRow = {
    pass: string;
    wire_bytes: { ts: number; rust: number; equal: boolean; first_diff_byte: number | null };
    wire_structure: {
        ts: Array<{ role: string; blocks: string[] }>;
        rust: Array<{ role: string; blocks: string[] }>;
    };
    empty_content_shapes: ValueSpaceAxis;
    dropped_placeholder_shapes: ValueSpaceAxis;
    reasoning_signature_shapes: ValueSpaceAxis;
};

export type PairedReplayResult = {
    fixture: ReplayFixture["source"];
    provider_id: string;
    sessions: { ts: string; rust: string };
    passes: ReplayDivergenceRow[];
    divergence_count: number;
    unadjudicated_divergence_count: number;
};

type ValueSpace = {
    empty_content_shapes: string[];
    dropped_placeholder_shapes: string[];
    reasoning_signature_shapes: string[];
};

const DROPPED = /^\[dropped(?: §\d+§)?\]$/u;
const MC_TAG_PREFIX = /^§\d+§\s*/u;

function exactBytes(prefix: string, byteLength: number, fill = "x"): string {
    if (byteLength <= 0) return "";
    const clipped = Buffer.from(prefix).subarray(0, byteLength).toString();
    return `${clipped}${fill.repeat(Math.max(0, byteLength - Buffer.byteLength(clipped)))}`;
}

function materializeBlock(block: ReplayBlockShape, label: string): Record<string, unknown> {
    if (block.type === "text") {
        const text = block.empty
            ? ""
            : block.dropped_tag !== undefined
              ? block.dropped_tag === 0
                  ? "[dropped]"
                  : `[dropped §${block.dropped_tag}§]`
              : exactBytes(`[[${label}:assistant]]`, block.text_bytes ?? 0, "a");
        return { type: "text", text };
    }
    if (block.type === "redacted_thinking") {
        return {
            type: "redacted_thinking",
            data: exactBytes("fixture-redacted", block.thinking_bytes ?? 0, "r"),
        };
    }
    return {
        type: "thinking",
        thinking: exactBytes(`[[${label}:thinking]]`, block.thinking_bytes ?? 0, "t"),
        signature: exactBytes("fixture-signature", block.signature_bytes ?? 0, "s"),
    };
}

function responseFor(pass: ReplayPassShape): MockResponse {
    return {
        content: pass.response.blocks.map((block) => materializeBlock(block, pass.label)),
        usage: {
            input_tokens: pass.response.input_tokens,
            output_tokens: pass.response.output_tokens,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        },
    };
}

function promptFor(pass: ReplayPassShape): string {
    return exactBytes(`[[replay:${pass.label}]]`, pass.input_text_bytes, "u");
}

function wireStructure(serialized: string): Array<{ role: string; blocks: string[] }> {
    const parsed = JSON.parse(serialized) as Array<{ role?: unknown; content?: unknown }>;
    return parsed.map((message) => {
        const role = typeof message.role === "string" ? message.role : "unknown";
        if (typeof message.content === "string") {
            return { role, blocks: [`string:${Buffer.byteLength(message.content)}`] };
        }
        if (!Array.isArray(message.content)) return { role, blocks: ["missing"] };
        return {
            role,
            blocks: message.content.map((rawBlock) => {
                if (!rawBlock || typeof rawBlock !== "object") return typeof rawBlock;
                const block = rawBlock as Record<string, unknown>;
                const type = typeof block.type === "string" ? block.type : "unknown";
                const payload =
                    typeof block.text === "string"
                        ? block.text
                        : typeof block.thinking === "string"
                          ? block.thinking
                          : typeof block.data === "string"
                            ? block.data
                            : "";
                const suffix =
                    typeof block.signature === "string" || typeof block.data === "string"
                        ? ":signed"
                        : "";
                const dropped =
                    typeof block.text === "string" && DROPPED.test(block.text.trim())
                        ? ":isolated_dropped"
                        : "";
                return `${type}:${Buffer.byteLength(payload)}${suffix}${dropped}`;
            }),
        };
    });
}

function classifyWire(serialized: string): ValueSpace {
    const parsed = JSON.parse(serialized) as Array<{ role?: unknown; content?: unknown }>;
    const empty = new Set<string>();
    const dropped = new Set<string>();
    const reasoning = new Set<string>();

    for (const message of parsed) {
        const role = typeof message.role === "string" ? message.role : "unknown";
        if (message.content === "") empty.add(`${role}:content=empty_string`);
        if (Array.isArray(message.content) && message.content.length === 0) {
            empty.add(`${role}:content=empty_array`);
        }
        if (!Array.isArray(message.content)) continue;
        for (const [index, rawBlock] of message.content.entries()) {
            if (!rawBlock || typeof rawBlock !== "object") continue;
            const block = rawBlock as Record<string, unknown>;
            const type = typeof block.type === "string" ? block.type : "unknown";
            const text = typeof block.text === "string" ? block.text : null;
            if (text === "") empty.add(`${role}:${type}.text=empty_string`);
            const normalizedText = text?.trim().replace(MC_TAG_PREFIX, "").trim() ?? null;
            if (normalizedText !== null && DROPPED.test(normalizedText)) {
                dropped.add(`${role}:isolated_dropped_placeholder`);
            } else if (text !== null && text.includes("[dropped")) {
                dropped.add(`${role}:embedded_dropped_placeholder`);
            }
            if (role === "assistant" && (type === "thinking" || type === "redacted_thinking")) {
                const signature =
                    (typeof block.signature === "string" && block.signature.length > 0) ||
                    (typeof block.data === "string" && block.data.length > 0);
                reasoning.add(
                    `${type}:${index === 0 ? "index_0" : "nonzero_index"}:${signature ? "signed" : "unsigned"}`,
                );
            }
        }
    }

    return {
        empty_content_shapes: [...empty].sort(),
        dropped_placeholder_shapes: [...dropped].sort(),
        reasoning_signature_shapes: [...reasoning].sort(),
    };
}

function axis(
    ts: string[],
    rust: string[],
    adjudication?: NonNullable<ReplayFixture["adjudications"]>[number],
): ValueSpaceAxis {
    const tsSet = new Set(ts);
    const rustSet = new Set(rust);
    const tsOnly = ts.filter((value) => !rustSet.has(value));
    const rustOnly = rust.filter((value) => !tsSet.has(value));
    const matchesAdjudication =
        adjudication !== undefined &&
        JSON.stringify(tsOnly) === JSON.stringify(adjudication.ts_only) &&
        JSON.stringify(rustOnly) === JSON.stringify(adjudication.rust_only);
    return {
        classification:
            tsOnly.length === 0 && rustOnly.length === 0
                ? "matched_value_space"
                : "divergent_value_space",
        ts_only: tsOnly,
        rust_only: rustOnly,
        shared: ts.filter((value) => rustSet.has(value)),
        ...(matchesAdjudication
            ? {
                  adjudication: {
                      decision: adjudication.decision,
                      reason: adjudication.reason,
                      source: adjudication.source,
                  },
              }
            : {}),
    };
}

function firstDiffByte(left: string, right: string): number | null {
    const a = Buffer.from(left);
    const b = Buffer.from(right);
    const limit = Math.min(a.length, b.length);
    for (let index = 0; index < limit; index += 1) {
        if (a[index] !== b[index]) return index;
    }
    return a.length === b.length ? null : limit;
}

export function comparePairedReplayPasses(
    fixture: ReplayFixture,
    tsWires: string[],
    rustWires: string[],
): ReplayDivergenceRow[] {
    if (tsWires.length !== fixture.passes.length || rustWires.length !== fixture.passes.length) {
        throw new Error(
            `replay capture count mismatch: fixture=${fixture.passes.length} ts=${tsWires.length} rust=${rustWires.length}`,
        );
    }
    return fixture.passes.map((pass, index) => {
        const tsWire = tsWires[index]!;
        const rustWire = rustWires[index]!;
        const tsValueSpace = classifyWire(tsWire);
        const rustValueSpace = classifyWire(rustWire);
        const reasoningAdjudication = fixture.adjudications?.find(
            (entry) => entry.pass === pass.label && entry.axis === "reasoning_signature_shapes",
        );
        return {
            pass: pass.label,
            wire_bytes: {
                ts: Buffer.byteLength(tsWire),
                rust: Buffer.byteLength(rustWire),
                equal: tsWire === rustWire,
                first_diff_byte: firstDiffByte(tsWire, rustWire),
            },
            wire_structure: {
                ts: wireStructure(tsWire),
                rust: wireStructure(rustWire),
            },
            empty_content_shapes: axis(
                tsValueSpace.empty_content_shapes,
                rustValueSpace.empty_content_shapes,
            ),
            dropped_placeholder_shapes: axis(
                tsValueSpace.dropped_placeholder_shapes,
                rustValueSpace.dropped_placeholder_shapes,
            ),
            reasoning_signature_shapes: axis(
                tsValueSpace.reasoning_signature_shapes,
                rustValueSpace.reasoning_signature_shapes,
                reasoningAdjudication,
            ),
        };
    });
}

function loadFixture(path: string): ReplayFixture {
    const parsed = JSON.parse(readFileSync(path, "utf8")) as ReplayFixture;
    if (parsed.schema !== 1 || !Array.isArray(parsed.passes) || parsed.passes.length === 0) {
        throw new Error(`invalid paired replay fixture: ${path}`);
    }
    return parsed;
}

async function driveLane(
    harness: RustTestHarness,
    fixture: ReplayFixture,
): Promise<{ sessionId: string; wires: string[] }> {
    const sessionId = await harness.createSession();
    const wires: string[] = [];
    for (const pass of fixture.passes) {
        harness.mock.setDefault(responseFor(pass));
        await harness.sendPrompt(sessionId, promptFor(pass));
        wires.push(harness.lastMainWireSerialized());
    }
    return { sessionId, wires };
}

export async function runPairedSessionReplay(options: {
    fixturePath?: string;
    providerID?: string;
} = {}): Promise<PairedReplayResult> {
    const fixturePath =
        options.fixturePath ?? resolve(import.meta.dir, "../fixtures/parity-hunt-14-session-shape.json");
    const fixture = loadFixture(fixturePath);
    const providerID = options.providerID ?? "anthropic";
    const harness = await RustTestHarness.create({
        startInTsMode: true,
        startHistorianProducer: false,
        providerID,
        modelContextLimit: 200_000,
        magicContextConfig: {
            execute_threshold_percentage: 95,
            memory: { auto_search: { enabled: false } },
            compressor: { enabled: false },
        },
    });
    try {
        const ts = await driveLane(harness, fixture);
        harness.mock.reset();
        await harness.restart({
            rust: true,
            magicContextConfig: {
                execute_threshold_percentage: 95,
                memory: { auto_search: { enabled: false } },
                compressor: { enabled: false },
            },
        });
        const rust = await driveLane(harness, fixture);
        const passes = comparePairedReplayPasses(fixture, ts.wires, rust.wires);
        const divergentAxes = passes.flatMap((pass) => [
            pass.empty_content_shapes,
            pass.dropped_placeholder_shapes,
            pass.reasoning_signature_shapes,
        ]).filter((entry) => entry.classification === "divergent_value_space");
        return {
            fixture: fixture.source,
            provider_id: providerID,
            sessions: {
                ts: createHash("sha256").update(ts.sessionId).digest("hex").slice(0, 12),
                rust: createHash("sha256").update(rust.sessionId).digest("hex").slice(0, 12),
            },
            passes,
            divergence_count: divergentAxes.length,
            unadjudicated_divergence_count: divergentAxes.filter(
                (entry) => entry.adjudication === undefined,
            ).length,
        };
    } finally {
        await harness.dispose();
    }
}
