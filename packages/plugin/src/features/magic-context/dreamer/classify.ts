import { DREAMER_CLASSIFIER_AGENT } from "../../../agents/dreamer";
import { isRustAuthorityDrainingError } from "../../../plugin/rust-tool-backends";
import type { PluginContext } from "../../../plugin/types";
import * as shared from "../../../shared";
import {
    extractLatestAssistantText,
    hasLengthCappedOutput,
} from "../../../shared/assistant-message-extractor";
import { describeError, getErrorMessage } from "../../../shared/error-message";
import { shouldKeepSubagents } from "../../../shared/keep-subagents";
import { log } from "../../../shared/logger";
import { hasShareabilitySensitiveText } from "../../../shared/redaction";
import { modelBodyField } from "../../../shared/resolve-fallbacks";
import type { Database } from "../../../shared/sqlite";
import {
    getMemoriesByProject,
    getUnclassifiedMemoryIds,
    type Memory,
    setMemoryClassification,
} from "../memory";
import { recordChildInvocation } from "../subagent-token-capture";
import {
    buildClassifyPrompt,
    CLASSIFY_SYSTEM_PROMPT,
    type ClassifyAnchorMemory,
    type ClassifyPromptMemory,
    parseClassifyManifest,
} from "./classify-prompt";
import { runLeaseGuardedWrite, startLeaseHeartbeat } from "./lease";
import { assertManifestCoversExactly } from "./manifest-parser";

/**
 * classify-memories: a NON-agentic single-shot transform. Scores each project
 * memory's importance / scope / shareability from its TEXT (no code reads), then
 * the HOST batch-applies the columns via setMemoryClassification — cache-neutral.
 *
 * 3-stage anchoring (hardcoded 10/100 thresholds):
 *  - Stage 1 (< 10 memories): skip — too small a pool to score meaningfully.
 *  - Stage 2 (<= 100): classify the WHOLE pool every run (the model sees the full
 *    distribution, so it can discriminate). No anchors.
 *  - Stage 3 (> 100): classify only the NEW/CHANGED memories (classified_at NULL),
 *    plus a stratified sample of already-classified memories as scoring ANCHORS
 *    (calibration, not re-scored).
 *
 * The classified_at marker (stamped by setMemoryClassification, cleared on content
 * change) is the per-memory run-gate: a classified memory is not re-scored.
 */

const MIN_POOL_TO_CLASSIFY = 10; // Stage 1 floor
const FULL_POOL_CEILING = 100; // Stage 2 ceiling (<= → classify all)
const STAGE3_ANCHOR_COUNT = 30; // calibration anchors shown in Stage 3
// Even Stage 2/3 chunk so peak context stays bounded on a 128K window. 100
// memories ≈ 8.6K tokens of pool text + guidance — comfortably one chunk; a
// >100 to-classify Stage-3 backlog splits into chunks of this size.
const CLASSIFY_CHUNK_SIZE = 100;

export interface ClassifyModuleCallArgs {
    sessionId: string;
    projectRoot: string;
    method: string;
    body: unknown;
    signal?: AbortSignal;
}

export interface ClassifyModuleClient {
    call(args: ClassifyModuleCallArgs): Promise<unknown>;
}

export interface ClassifyArgs {
    db: Database;
    client: PluginContext["client"];
    projectIdentity: string;
    parentSessionId: string | undefined;
    sessionDirectory: string;
    holderId: string;
    leaseKey: string;
    deadline: number;
    model?: string;
    fallbackModels?: readonly string[];
    /** Present only for rust-mode projects whose memories authority is MODULE. */
    moduleClient?: ClassifyModuleClient;
    moduleSessionId?: string;
    moduleProjectRoot?: string;
    moduleContextStoreUuid?: string;
    moduleAuthorityGeneration?: number;
    moduleCommandId?: string;
}

export interface ClassifyResult {
    classified: number;
    changed: number;
    chunks: number;
    stage: 1 | 2 | 3;
}

function toPromptMemory(m: Memory): ClassifyPromptMemory {
    return {
        id: m.id,
        category: m.category,
        content: m.content,
        importance: m.importance ?? 50,
        scope: m.scope ?? "project",
        shareable: m.shareable ?? false,
    };
}

/** Stratified sample of already-classified memories across importance bands, so
 *  Stage-3 anchors span the full distribution rather than clustering. */
function stratifiedAnchors(classified: Memory[], count: number): ClassifyAnchorMemory[] {
    if (classified.length <= count) {
        return classified.map((m) => ({
            id: m.id,
            category: m.category,
            content: m.content,
            importance: m.importance ?? 50,
        }));
    }
    const sorted = [...classified].sort((a, b) => (a.importance ?? 50) - (b.importance ?? 50));
    const step = sorted.length / count;
    const out: ClassifyAnchorMemory[] = [];
    for (let i = 0; i < count; i += 1) {
        const m = sorted[Math.min(sorted.length - 1, Math.floor(i * step))];
        out.push({
            id: m.id,
            category: m.category,
            content: m.content,
            importance: m.importance ?? 50,
        });
    }
    return out;
}

export async function runClassify(args: ClassifyArgs): Promise<ClassifyResult> {
    const active = getMemoriesByProject(args.db, args.projectIdentity);

    // Stage 1: too small a pool to score meaningfully.
    if (active.length < MIN_POOL_TO_CLASSIFY) {
        return { classified: 0, changed: 0, chunks: 0, stage: 1 };
    }

    let stage: 2 | 3;
    let toClassify: Memory[];
    let anchors: ClassifyAnchorMemory[] = [];
    if (active.length <= FULL_POOL_CEILING) {
        // Stage 2: classify the whole pool every run.
        stage = 2;
        toClassify = active;
    } else {
        // Stage 3: only the new/changed (unclassified) memories, with stratified
        // already-classified anchors for distribution calibration.
        stage = 3;
        const unclassifiedIds = new Set(
            getUnclassifiedMemoryIds(
                args.db,
                active.map((m) => m.id),
            ),
        );
        toClassify = active.filter((m) => unclassifiedIds.has(m.id));
        const classified = active.filter((m) => !unclassifiedIds.has(m.id));
        anchors = stratifiedAnchors(classified, STAGE3_ANCHOR_COUNT);
    }

    const result: ClassifyResult = { classified: 0, changed: 0, chunks: 0, stage };
    if (toClassify.length === 0) {
        log(`[dreamer] classify: stage=${stage} nothing to classify`);
        return result;
    }

    const chunks: Memory[][] = [];
    for (let i = 0; i < toClassify.length; i += CLASSIFY_CHUNK_SIZE) {
        chunks.push(toClassify.slice(i, i + CLASSIFY_CHUNK_SIZE));
    }

    const abortController = new AbortController();
    const heartbeat = startLeaseHeartbeat(args.db, args.holderId, args.leaseKey, () =>
        abortController.abort(),
    );

    try {
        for (let i = 0; i < chunks.length; i += 1) {
            const remainingMs = Math.max(0, args.deadline - Date.now());
            if (remainingMs <= 0) break;
            const chunksRemaining = chunks.length - i;
            const sliceMs = Math.max(1, Math.floor(remainingMs / chunksRemaining));

            const counts = await classifyOneChunk(
                args,
                chunks[i],
                anchors,
                sliceMs,
                abortController.signal,
            );
            result.classified += counts.classified;
            result.changed += counts.changed;
            result.chunks += 1;
        }
        log(
            `[dreamer] classify: stage=${stage} classified=${result.classified} changed=${result.changed} chunks=${result.chunks}`,
        );
        return result;
    } finally {
        heartbeat.stop();
    }
}

async function classifyOneChunk(
    args: ClassifyArgs,
    chunk: Memory[],
    anchors: ClassifyAnchorMemory[],
    sliceMs: number,
    signal: AbortSignal,
): Promise<{ classified: number; changed: number }> {
    let agentSessionId: string | null = null;
    const startedAt = Date.now();
    try {
        const prompt = buildClassifyPrompt({
            projectPath: args.projectIdentity,
            memories: chunk.map(toPromptMemory),
            anchors,
        });
        if (
            args.moduleClient &&
            args.moduleSessionId &&
            args.moduleProjectRoot &&
            args.moduleContextStoreUuid &&
            args.moduleAuthorityGeneration !== undefined
        ) {
            const run = await runClassifyThroughModule(args, chunk, anchors, signal);
            recordInvocation(args, startedAt, { status: "completed" });
            return run;
        }

        const createResponse = await args.client.session.create({
            body: {
                ...(args.parentSessionId ? { parentID: args.parentSessionId } : {}),
                title: "magic-context-dream-classify",
            },
            query: { directory: args.sessionDirectory },
        });
        const created = shared.normalizeSDKResponse(
            createResponse,
            null as { id?: string } | null,
            {
                preferResponseOnMissingData: true,
            },
        );
        agentSessionId = typeof created?.id === "string" ? created.id : null;
        if (!agentSessionId) throw new Error("Could not create classify session.");

        const run = await shared.promptSyncWithValidatedOutputRetry(
            args.client,
            {
                path: { id: agentSessionId },
                query: { directory: args.sessionDirectory },
                body: {
                    agent: DREAMER_CLASSIFIER_AGENT,
                    system: CLASSIFY_SYSTEM_PROMPT,
                    ...modelBodyField(args.model),
                    parts: [{ type: "text", text: prompt, synthetic: true }],
                },
            },
            {
                timeoutMs: sliceMs,
                signal,
                fallbackModels: args.fallbackModels,
                callContext: "dreamer:classify-memories",
                fetchOutput: async () => {
                    const messagesResponse = await args.client.session.messages({
                        path: { id: agentSessionId as string },
                        query: { directory: args.sessionDirectory, limit: 50 },
                    });
                    return shared.normalizeSDKResponse(messagesResponse, [] as unknown[], {
                        preferResponseOnMissingData: true,
                    });
                },
                validateOutput: (messages) => {
                    if (hasLengthCappedOutput(messages)) {
                        throw new Error("classify returned length-capped output");
                    }
                    const text = extractLatestAssistantText(messages);
                    if (!text) throw new Error("classify returned no output");
                    parseClassifyManifest(text);
                    return text;
                },
            },
        );

        recordInvocation(args, startedAt, { status: "completed", messages: run.output });
        return applyClassifications(args, chunk, run.validated);
    } catch (error) {
        const desc = describeError(error);
        log(
            `[dreamer] classify chunk failed: ${desc.brief}`,
            desc.stackHead ? { stackHead: desc.stackHead } : undefined,
        );
        recordInvocation(args, startedAt, { status: "failed", error });
        if (signal.aborted) throw error;
        return { classified: 0, changed: 0 };
    } finally {
        // Delete on success AND failure (the failed child still holds the
        // memory-pool snapshot from the prompt). keep_subagents still honored —
        // memory-pool text, not raw user transcripts.
        if (agentSessionId && !shouldKeepSubagents()) {
            await args.client.session
                .delete({
                    path: { id: agentSessionId },
                    query: { directory: args.sessionDirectory },
                })
                .catch((e: unknown) => {
                    log(`[dreamer] classify session cleanup failed: ${getErrorMessage(e)}`);
                });
        }
    }
}

/** Apply the manifest host-side: the manifest must cover exactly this chunk;
 *  shareable fails closed against sensitive text. setMemoryClassification stamps
 *  classified_at (the run-gate) and is cache-neutral. */
async function runClassifyThroughModule(
    args: ClassifyArgs,
    chunk: Memory[],
    anchors: ClassifyAnchorMemory[],
    signal: AbortSignal,
): Promise<{ classified: number; changed: number }> {
    const prompt = buildClassifyPrompt({
        projectPath: args.projectIdentity,
        memories: chunk.map(toPromptMemory),
        anchors,
    });
    const response = await args.moduleClient?.call({
        sessionId: args.moduleSessionId as string,
        projectRoot: args.moduleProjectRoot as string,
        method: "dreamer.run_task",
        body: {
            method: "dreamer.run_task",
            v: 1,
            session_id: args.moduleSessionId,
            task: "classify",
            command_id: `classify:${args.moduleCommandId ?? Date.now()}:${chunk.map((m) => m.id).join(",")}`,
            authority_generation: args.moduleAuthorityGeneration,
            payload: {
                prompt_body: prompt,
                items: chunk.map((memory) => ({
                    memory_id: memory.id,
                    content_hash: memory.normalizedHash,
                })),
            },
        },
        signal,
    });
    const result = (response as { result?: unknown } | null)?.result ?? response;
    if (!result || typeof result !== "object")
        throw new Error("module returned invalid classify result");
    const manifestText = (result as { manifest_text?: unknown }).manifest_text;
    if (typeof manifestText !== "string") throw new Error("module returned no classify manifest");
    if ((result as { truncated?: unknown }).truncated === true) {
        throw new Error("classify returned length-capped output");
    }
    const parsed = parseClassifyManifest(manifestText);
    assertManifestCoversExactly(
        parsed.map((entry) => entry.id),
        new Set(chunk.map((memory) => memory.id)),
        "classify",
    );
    const rows = parsed.map((entry) => {
        const memory = chunk.find((candidate) => candidate.id === entry.id);
        if (!memory) throw new Error(`classify returned unknown memory ${entry.id}`);
        return {
            memory_id: entry.id,
            content_hash_at_prompt: memory.normalizedHash,
            importance: entry.importance,
            scope: entry.scope,
            // The host forces shareable to false whenever the memory text is sensitive,
            // regardless of whether classification runs through the module or provider path.
            shareable:
                entry.shareable === true && hasShareabilitySensitiveText(memory.content)
                    ? false
                    : entry.shareable,
        };
    });
    let applied: unknown;
    try {
        applied = await args.moduleClient?.call({
            sessionId: args.moduleSessionId as string,
            projectRoot: args.moduleProjectRoot as string,
            method: "memory.set_classification",
            body: {
                name: "memory.set_classification",
                arguments: {
                    memory_project: args.projectIdentity,
                    context_store_uuid: args.moduleContextStoreUuid,
                    authority_generation: args.moduleAuthorityGeneration,
                    rows,
                },
            },
            signal,
        });
    } catch (error) {
        if (isRustAuthorityDrainingError(error)) {
            throw new Error("Rust memory authority is not ready; TypeScript fallback is disabled.");
        }
        throw error;
    }
    if (isRustAuthorityDrainingError(applied)) {
        throw new Error("Rust memory authority is not ready; TypeScript fallback is disabled.");
    }
    const applyResult = (applied as { result?: unknown } | null)?.result ?? applied;
    if (!applyResult || typeof applyResult !== "object") {
        throw new Error("module returned invalid classification apply result");
    }
    const accepted = (applyResult as { accepted?: unknown }).accepted;
    if (!Array.isArray(accepted))
        throw new Error("module returned no classification acceptance list");
    const stale = (applyResult as { rejected?: unknown }).rejected;
    if (Array.isArray(stale)) {
        const unexpected = stale.some(
            (row) =>
                row && typeof row === "object" && (row as { reason?: unknown }).reason !== "stale",
        );
        if (unexpected) throw new Error("module rejected classification for a non-stale reason");
    }
    return { classified: accepted.length, changed: accepted.length };
}

export function applyClassifications(
    args: ClassifyArgs,
    chunk: Memory[],
    manifestText: string,
): { classified: number; changed: number } {
    const byId = new Map(chunk.map((m) => [m.id, m]));
    const parsed = parseClassifyManifest(manifestText);
    assertManifestCoversExactly(
        parsed.map((entry) => entry.id),
        new Set(byId.keys()),
        "classify",
    );
    if (parsed.length === 0) return { classified: 0, changed: 0 };

    let classified = 0;
    let changed = 0;
    runLeaseGuardedWrite(args.db, args.holderId, args.leaseKey, () => {
        for (const p of parsed) {
            const memory = byId.get(p.id);
            if (!memory) continue;
            // Fail closed: secret/credential/personal-path text is forced private
            // regardless of the model's verdict.
            const shareable =
                p.shareable === true && hasShareabilitySensitiveText(memory.content)
                    ? false
                    : p.shareable;
            const didChange = setMemoryClassification(args.db, p.id, {
                importance: p.importance,
                scope: p.scope,
                shareable,
            });
            classified += 1; // stamped classified_at (run-gate satisfied)
            if (didChange) changed += 1; // an actual column value moved
        }
    });
    return { classified, changed };
}

function recordInvocation(
    args: ClassifyArgs,
    startedAt: number,
    params: { status: "completed" | "failed"; messages?: unknown[]; error?: unknown },
): void {
    if (!args.parentSessionId) return;
    recordChildInvocation({
        db: args.db,
        parentSessionId: args.parentSessionId,
        harness: "opencode",
        subagent: "dreamer",
        task: "classify-memories",
        startedAt,
        status: params.status,
        messages: params.messages,
        error: params.error,
    });
}
