import { MockProvider, type MockUsage } from "./mock-provider/server";

export type MidTurnArm = "hold" | "apply";
export type CacheTtl = "5m" | "1h";

type JsonRecord = Record<string, unknown>;

export interface MidTurnExperimentOptions {
    steps?: number;
    applyAtStep?: number;
    dropChars?: number;
    ttl?: CacheTtl;
    model?: string;
}

export interface PrefixMeasurement {
    stableBytes: number;
    cacheReadBytes: number;
    cacheCreationBytes: number;
    firstDivergenceByte: number;
}

export interface StepUsageRecord extends PrefixMeasurement {
    arm: MidTurnArm;
    step: number;
    mutated: boolean;
    inputTokens: number;
    cacheReadTokens: number;
    cacheCreationTokens: number;
}

export interface AbExperimentResult {
    mode: "mock" | "live";
    options: Required<MidTurnExperimentOptions>;
    hold: StepUsageRecord[];
    apply: StepUsageRecord[];
}

const DEFAULT_OPTIONS: Required<MidTurnExperimentOptions> = {
    steps: 7,
    applyAtStep: 4,
    dropChars: 40_000,
    ttl: "5m",
    model: "claude-sonnet-4-6",
};

function resolvedOptions(options: MidTurnExperimentOptions = {}): Required<MidTurnExperimentOptions> {
    const resolved: Required<MidTurnExperimentOptions> = {
        steps: options.steps ?? DEFAULT_OPTIONS.steps,
        applyAtStep: options.applyAtStep ?? DEFAULT_OPTIONS.applyAtStep,
        dropChars: options.dropChars ?? DEFAULT_OPTIONS.dropChars,
        ttl: options.ttl ?? DEFAULT_OPTIONS.ttl,
        model: options.model ?? DEFAULT_OPTIONS.model,
    };
    if (!Number.isInteger(resolved.steps) || resolved.steps < 3) {
        throw new Error("steps must be an integer >= 3");
    }
    if (
        !Number.isInteger(resolved.applyAtStep) ||
        resolved.applyAtStep < 2 ||
        resolved.applyAtStep >= resolved.steps
    ) {
        throw new Error("applyAtStep must be between 2 and steps - 1");
    }
    if (!Number.isInteger(resolved.dropChars) || resolved.dropChars < 8_000) {
        throw new Error("dropChars must be an integer >= 8000 so live prompts cross cache minima");
    }
    return resolved;
}

function stripCacheControl(value: unknown): unknown {
    if (Array.isArray(value)) return value.map(stripCacheControl);
    if (value === null || typeof value !== "object") return value;
    const output: JsonRecord = {};
    for (const [key, child] of Object.entries(value as JsonRecord)) {
        if (key === "cache_control" || key === "cacheControl") continue;
        output[key] = stripCacheControl(child);
    }
    return output;
}

export function stablePromptBytes(body: JsonRecord): Buffer {
    return cacheSequence(body).prompt;
}

function cacheSequence(body: JsonRecord): { prompt: Buffer; breakpoints: number[] } {
    const chunks: string[] = [];
    const breakpoints: number[] = [];
    let byteLength = 0;
    const push = (kind: string, role: string, block: unknown): void => {
        const record = block !== null && typeof block === "object" ? (block as JsonRecord) : undefined;
        const chunk = `${JSON.stringify([kind, role, stripCacheControl(block)])}\n`;
        chunks.push(chunk);
        byteLength += Buffer.byteLength(chunk);
        if (record?.cache_control !== undefined || record?.cacheControl !== undefined) {
            breakpoints.push(byteLength);
        }
    };

    for (const tool of (body.tools as unknown[] | undefined) ?? []) push("tool", "tool", tool);
    for (const block of (body.system as unknown[] | undefined) ?? []) push("system", "system", block);
    for (const message of (body.messages as JsonRecord[] | undefined) ?? []) {
        const role = String(message.role ?? "unknown");
        if (Array.isArray(message.content)) {
            for (const block of message.content) push("message", role, block);
        } else {
            push("message", role, message.content);
        }
    }

    return { prompt: Buffer.from(chunks.join("")), breakpoints };
}

export function commonPrefixBytes(left: Uint8Array, right: Uint8Array): number {
    const length = Math.min(left.byteLength, right.byteLength);
    let index = 0;
    while (index < length && left[index] === right[index]) index += 1;
    return index;
}

/**
 * Deterministic byte oracle for the provider's exact-prefix contract.
 *
 * Only prefixes ending at an explicit cache marker are reusable. After an old
 * message changes, the tools-plus-system marker survives and the revised final
 * marker makes the whole new prefix reusable by the next extending request.
 */
export class PrefixCacheOracle {
    private readonly cachedPrefixes: Buffer[] = [];
    private previous: Buffer | undefined;

    measure(body: JsonRecord): PrefixMeasurement {
        const { prompt: current, breakpoints } = cacheSequence(body);
        let cacheReadBytes = 0;
        for (const prior of this.cachedPrefixes) {
            if (commonPrefixBytes(prior, current) === prior.byteLength) {
                cacheReadBytes = Math.max(cacheReadBytes, prior.byteLength);
            }
        }
        const firstDivergenceByte = this.previous
            ? commonPrefixBytes(this.previous, current)
            : 0;
        for (const breakpoint of breakpoints) {
            this.cachedPrefixes.push(current.subarray(0, breakpoint));
        }
        this.previous = current;
        return {
            stableBytes: current.byteLength,
            cacheReadBytes,
            cacheCreationBytes: current.byteLength - cacheReadBytes,
            firstDivergenceByte,
        };
    }
}

function ballast(chars: number): string {
    const seed = "alpha bravo cache delta evidence foxtrot golf hotel india juliet ";
    return `DROP-BEGIN\n${seed.repeat(Math.ceil(chars / seed.length)).slice(0, chars)}\nDROP-END`;
}

function contentText(text: string, cacheControl?: { type: "ephemeral"; ttl: CacheTtl }) {
    return [{ type: "text", text, ...(cacheControl ? { cache_control: cacheControl } : {}) }];
}

export function buildMidTurnRequest(
    arm: MidTurnArm,
    step: number,
    options: Required<MidTurnExperimentOptions>,
    namespace: string,
): JsonRecord {
    const applied = arm === "apply" && step >= options.applyAtStep;
    const messages: JsonRecord[] = [
        { role: "user", content: contentText(`scratch session ${namespace}`) },
        { role: "assistant", content: contentText("I will inspect the fixture with tools.") },
        {
            role: "user",
            content: contentText(applied ? "[dropped by mid-turn A/B instrument]" : ballast(options.dropChars)),
        },
        { role: "assistant", content: contentText("The large fixture is now part of served history.") },
    ];

    for (let toolStep = 1; toolStep <= step; toolStep += 1) {
        const toolUseId = `toolu_mid_turn_ab_${String(toolStep).padStart(3, "0")}`;
        messages.push({
            role: "assistant",
            content: [
                {
                    type: "tool_use",
                    id: toolUseId,
                    name: "inspect_fixture",
                    input: { step: toolStep, path: `fixture-${toolStep}.txt` },
                },
            ],
        });
        messages.push({
            role: "user",
            content: [
                {
                    type: "tool_result",
                    tool_use_id: toolUseId,
                    content: `tool result ${toolStep}: ${"result ".repeat(24)}`,
                    ...(toolStep === step
                        ? { cache_control: { type: "ephemeral", ttl: options.ttl } }
                        : {}),
                },
            ],
        });
    }

    return {
        model: options.model,
        max_tokens: 16,
        system: contentText(
            `You are running a deterministic cache experiment (${namespace}). ` +
                "Return OK after reading each tool result. ".repeat(48),
            { type: "ephemeral", ttl: options.ttl },
        ),
        tools: [
            {
                name: "inspect_fixture",
                description: "Return deterministic fixture metadata.",
                input_schema: {
                    type: "object",
                    properties: { step: { type: "integer" }, path: { type: "string" } },
                    required: ["step", "path"],
                },
            },
        ],
        messages,
        stream: false,
    };
}

function tokensForBytes(bytes: number): number {
    return Math.ceil(bytes / 4);
}

async function postJson(url: string, body: JsonRecord, headers: Record<string, string> = {}) {
    const response = await fetch(url, {
        method: "POST",
        headers: { "content-type": "application/json", ...headers },
        body: JSON.stringify(body),
    });
    const text = await response.text();
    if (!response.ok) throw new Error(`provider HTTP ${response.status}: ${text.slice(0, 500)}`);
    return JSON.parse(text) as {
        usage?: {
            input_tokens?: number;
            cache_read_input_tokens?: number;
            cache_creation_input_tokens?: number;
        };
    };
}

async function runMockArm(
    arm: MidTurnArm,
    options: Required<MidTurnExperimentOptions>,
): Promise<StepUsageRecord[]> {
    const mock = new MockProvider();
    const { baseURL } = await mock.start();
    const oracle = new PrefixCacheOracle();
    const measurements: PrefixMeasurement[] = [];
    mock.addMatcher((body) => {
        const measurement = oracle.measure(body);
        measurements.push(measurement);
        const usage: MockUsage = {
            input_tokens: 0,
            output_tokens: 1,
            cache_read_input_tokens: tokensForBytes(measurement.cacheReadBytes),
            cache_creation_input_tokens: tokensForBytes(measurement.cacheCreationBytes),
        };
        return { text: "OK", usage };
    });

    const records: StepUsageRecord[] = [];
    try {
        for (let step = 1; step <= options.steps; step += 1) {
            const body = buildMidTurnRequest(arm, step, options, "mock-arm");
            const response = await postJson(`${baseURL}/v1/messages`, body);
            const measurement = measurements.at(-1);
            if (!measurement) throw new Error(`mock did not meter ${arm} step ${step}`);
            records.push({
                arm,
                step,
                mutated: arm === "apply" && step === options.applyAtStep,
                ...measurement,
                inputTokens: response.usage?.input_tokens ?? 0,
                cacheReadTokens: response.usage?.cache_read_input_tokens ?? 0,
                cacheCreationTokens: response.usage?.cache_creation_input_tokens ?? 0,
            });
        }
    } finally {
        await mock.stop();
    }
    return records;
}

export async function runMockAbExperiment(
    experimentOptions: MidTurnExperimentOptions = {},
): Promise<AbExperimentResult> {
    const options = resolvedOptions(experimentOptions);
    const [hold, apply] = await Promise.all([runMockArm("hold", options), runMockArm("apply", options)]);
    assertMockAbEvidence({ mode: "mock", options, hold, apply });
    return { mode: "mock", options, hold, apply };
}

export function assertMockAbEvidence(result: AbExperimentResult): void {
    const mutationIndex = result.options.applyAtStep - 1;
    for (let index = 0; index < mutationIndex; index += 1) {
        const hold = result.hold[index];
        const apply = result.apply[index];
        if (
            hold.stableBytes !== apply.stableBytes ||
            hold.cacheReadBytes !== apply.cacheReadBytes ||
            hold.cacheCreationBytes !== apply.cacheCreationBytes
        ) {
            throw new Error(`arms diverged before the intended mutation at step ${index + 1}`);
        }
    }

    const holdMutation = result.hold[mutationIndex];
    const applyMutation = result.apply[mutationIndex];
    if (applyMutation.firstDivergenceByte >= holdMutation.firstDivergenceByte) {
        throw new Error("apply arm did not move the first divergence into old served history");
    }
    if (applyMutation.cacheCreationBytes <= holdMutation.cacheCreationBytes) {
        throw new Error("apply arm did not recache a larger suffix on the mutation step");
    }

    const applyNext = result.apply[mutationIndex + 1];
    if (applyNext.cacheCreationBytes >= applyMutation.cacheCreationBytes) {
        throw new Error("revised prefix was not reusable on the step after the mutation");
    }
    if (applyNext.cacheReadBytes !== applyMutation.stableBytes) {
        throw new Error("post-mutation request did not reuse the complete revised prefix");
    }
}

function authHeaders(): Record<string, string> {
    const apiKey = process.env.ANTHROPIC_API_KEY?.trim();
    if (apiKey) return { "x-api-key": apiKey };
    const oauth = process.env.ANTHROPIC_AUTH_TOKEN?.trim();
    if (oauth) return { authorization: `Bearer ${oauth}` };
    throw new Error("live mode requires ANTHROPIC_API_KEY or ANTHROPIC_AUTH_TOKEN");
}

async function runLiveArm(
    arm: MidTurnArm,
    options: Required<MidTurnExperimentOptions>,
): Promise<StepUsageRecord[]> {
    const endpoint = process.env.ANTHROPIC_MESSAGES_URL?.trim() || "https://api.anthropic.com/v1/messages";
    const namespace = `live-${arm}-${crypto.randomUUID()}`;
    const headers = {
        ...authHeaders(),
        "anthropic-version": "2023-06-01",
        "anthropic-beta": process.env.ANTHROPIC_AUTH_TOKEN
            ? "oauth-2025-04-20,extended-cache-ttl-2025-04-11"
            : "extended-cache-ttl-2025-04-11",
        "user-agent": "magic-context-mid-turn-ab/1.0",
    };
    const records: StepUsageRecord[] = [];
    const oracle = new PrefixCacheOracle();
    for (let step = 1; step <= options.steps; step += 1) {
        const body = buildMidTurnRequest(arm, step, options, namespace);
        const measurement = oracle.measure(body);
        const response = await postJson(endpoint, body, headers);
        records.push({
            arm,
            step,
            mutated: arm === "apply" && step === options.applyAtStep,
            ...measurement,
            inputTokens: response.usage?.input_tokens ?? 0,
            cacheReadTokens: response.usage?.cache_read_input_tokens ?? 0,
            cacheCreationTokens: response.usage?.cache_creation_input_tokens ?? 0,
        });
    }
    return records;
}

export async function runLiveAnthropicAbExperiment(
    experimentOptions: MidTurnExperimentOptions = {},
): Promise<AbExperimentResult> {
    const options = resolvedOptions(experimentOptions);
    const hold = await runLiveArm("hold", options);
    const apply = await runLiveArm("apply", options);
    return { mode: "live", options, hold, apply };
}

export function formatAbTable(result: AbExperimentResult): string {
    const lines = [
        "step | hold read | hold create | apply read | apply create | apply recache bytes | event",
        "---: | ---: | ---: | ---: | ---: | ---: | :---",
    ];
    for (let index = 0; index < result.options.steps; index += 1) {
        const hold = result.hold[index];
        const apply = result.apply[index];
        lines.push(
            [
                hold.step,
                hold.cacheReadTokens,
                hold.cacheCreationTokens,
                apply.cacheReadTokens,
                apply.cacheCreationTokens,
                apply.cacheCreationBytes,
                apply.mutated ? "drop applied" : "append",
            ].join(" | "),
        );
    }
    return lines.join("\n");
}
