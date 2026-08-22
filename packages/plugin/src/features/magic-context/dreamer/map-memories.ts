import { createHash } from "node:crypto";

import { DREAMER_MEMORY_MAPPER_AGENT } from "../../../agents/dreamer";
import { createChildSessionWithFence } from "../../../hooks/magic-context/child-session-spawn";
import type { PluginContext } from "../../../plugin/types";
import * as shared from "../../../shared";
import {
    extractLatestAssistantText,
    hasLengthCappedOutput,
} from "../../../shared/assistant-message-extractor";
import { describeError, getErrorMessage } from "../../../shared/error-message";
import { shouldKeepSubagents } from "../../../shared/keep-subagents";
import { log } from "../../../shared/logger";
import type { ModelInput } from "../../../shared/model-resolution";
import { modelBodyField } from "../../../shared/resolve-fallbacks";
import type { Database } from "../../../shared/sqlite";
import {
    getMemoriesByProject,
    getMemoryVerifications,
    getUnmappedMemoryIds,
    normalizeVerificationFiles,
    recordMemoryMapping,
} from "../memory";
import { recordChildInvocation } from "../subagent-token-capture";
import { type LeaseAcquisition, runLeaseGuardedWrite, startLeaseHeartbeat } from "./lease";
import { assertManifestCoversExactly } from "./manifest-parser";
import {
    buildMapMemoriesPrompt,
    extractMemoryCandidatePaths,
    MAP_MEMORIES_SYSTEM_PROMPT,
    type MapMemoryInput,
    parseMapMemoriesManifest,
    validateMapMemoriesManifest,
} from "./map-memories-prompt";
import {
    DreamerModuleFailureError,
    type DreamerModuleRoute,
    getModuleMemoryIdentities,
} from "./module-apply";

/**
 * map-memories: ONE-TIME-style backfill that locates the backing file(s) for
 * every UNMAPPED project memory (or marks it file-independent), so the verify
 * task can run incrementally from the start (verify gates on "files changed
 * since THIS memory's verification" — which needs a mapping to exist).
 *
 * Self-maintaining: the gate is "unmapped memories exist", so the expensive
 * initial pool backfill happens once (across batches), then only the cheap
 * trickle of newly-added memories is mapped on later runs.
 *
 * Cost is bounded by the UNIQUE-FILE working set, not the memory count —
 * memories share files, so a large batch reads each hot file once and maps every
 * memory citing it in one turn. The shadow harness showed ~100 memories peaking
 * at ~100K context in ~41 turns (FASTER per-memory than 25), so we batch LARGE.
 * No max-turns (the agent's maxSteps cap is the only ceiling); a batch that
 * fails to emit a manifest simply leaves its memories unmapped for the next run.
 */

// Batch LARGE — chunking destroys file-read reuse. 80 keeps a batch comfortably
// under the agent's 60-step cap (harness: 100 memories ≈ 41 turns) with margin,
// and peak context well under a 128K window. A 200+ pool → ~3 batches.
const MAP_BATCH_SIZE = 80;

/**
 * Minimum wall-clock budget for one 80-memory agentic mapping batch. The mapper's
 * harness history needed about 41 turns for a 100-memory batch, but does not record
 * reliable wall time; mirror compress-cues' proven four-minute floor rather than
 * starting a batch with a deadline that cannot finish. Large backfills then bank
 * each committed batch and resume their remainder on a later run.
 */
export const MAP_BATCH_FLOOR_MS = 240_000;

/** Stop after two timeout-class failures: repeated timeouts mean this model cannot
 * finish the current mapping batch within its fair slice, so more attempts only
 * burn the deadline and quota without making the resumable backlog smaller. */
const CONSECUTIVE_TIMEOUT_LIMIT = 2;

/** Cap on already-mapped file-independent rows re-queued per run. A silent
 *  parse miss used to persist `independent=true` for memories that name real
 *  files; those rows never enter verify. Heal at most one extra batch so new
 *  unmapped memories stay first in line. */
export const MAX_INDEPENDENT_REQUEUE_PER_RUN = MAP_BATCH_SIZE;

export interface MapMemoriesArgs {
    db: Database;
    client: PluginContext["client"];
    projectIdentity: string;
    parentSessionId: string | undefined;
    sessionDirectory: string;
    holderId: string;
    leaseKey: string;
    deadline: number;
    leaseAcquisition?: LeaseAcquisition;
    model?: ModelInput;
    fallbackModels?: readonly ModelInput[];
    moduleRoute?: DreamerModuleRoute;
    onProgress?: (processed: number) => void;
}

export interface MapMemoriesResult {
    mapped: number;
    independent: number;
    /** Batches whose mappings were durably committed, not merely attempted. */
    batches: number;
    remaining: number;
    complete: boolean;
    /** Why a resumable run stopped before draining the selected input. */
    stopReason?: "deadline" | "timeout-circuit-breaker";
}

type MapBatchFailureClass = "timeout" | "other";

interface MapBatchOutcome {
    mapped: number;
    independent: number;
    /** Failed batches make no writes. The run loop needs the timeout class to
     * distinguish a model-too-slow starvation streak from ordinary retries. */
    failure?: {
        class: MapBatchFailureClass;
        elapsedMs: number;
    };
}

/** Evenly split the remaining deadline, but never starve an agentic mapping batch
 * below its floor. The caller first verifies that the remaining budget can fit the
 * floor, so the returned slice always fits the current deadline. */
export function computeMapBatchSliceMs(remainingMs: number, batchesRemaining: number): number {
    return Math.min(
        remainingMs,
        Math.max(MAP_BATCH_FLOOR_MS, Math.floor(remainingMs / batchesRemaining)),
    );
}

/** The shared prompt helper uses this exact error shape for a deadline expiry.
 * Validation and provider failures remain ordinary per-batch retries. */
function isTimeoutClassError(error: unknown): boolean {
    return error instanceof Error && /^prompt timed out after \d+ms$/.test(error.message);
}

/** Re-queue predicate: a file-independent mapping (sentinel, no real files)
 *  whose memory text names existing repo paths — the same seed heuristic the
 *  mapper already uses. That is the silent-corruption shape: a memory WITH
 *  backing files was persisted as independent. A legitimately-independent
 *  memory that names no existing path is a bystander and is left alone. */
export function shouldRequeueIndependentMapping(
    state: { hasSentinel: boolean; files: readonly string[] },
    content: string,
    repoDir: string,
): boolean {
    if (!state.hasSentinel || state.files.length > 0) return false;
    return extractMemoryCandidatePaths(content, repoDir).length > 0;
}

function toMapInput(
    memory: { id: number; category: string; content: string },
    repoDir: string,
): MapMemoryInput {
    return {
        id: memory.id,
        category: memory.category,
        content: memory.content,
        candidates: extractMemoryCandidatePaths(memory.content, repoDir),
    };
}

/** Unmapped active memories first, then a bounded heal of corrupted
 *  independent rows. Exported so tests can assert the re-queue predicate
 *  without standing up a child session. */
export function selectMapMemoryInputs(
    db: Database,
    projectIdentity: string,
    repoDir: string,
): MapMemoryInput[] {
    const active = getMemoriesByProject(db, projectIdentity);
    const activeIds = active.map((m) => m.id);
    const unmapped = new Set(getUnmappedMemoryIds(db, activeIds));
    const verifications = getMemoryVerifications(db, activeIds);

    const unmappedInputs = active
        .filter((m) => unmapped.has(m.id))
        .map((m) => toMapInput(m, repoDir));

    const requeue: MapMemoryInput[] = [];
    for (const memory of active) {
        if (unmapped.has(memory.id)) continue;
        const state = verifications.get(memory.id);
        if (!state) continue;
        if (!shouldRequeueIndependentMapping(state, memory.content, repoDir)) continue;
        requeue.push(toMapInput(memory, repoDir));
        if (requeue.length >= MAX_INDEPENDENT_REQUEUE_PER_RUN) break;
    }

    return [...unmappedInputs, ...requeue];
}

export async function mapMemories(args: MapMemoriesArgs): Promise<MapMemoriesResult> {
    const result: MapMemoriesResult = {
        mapped: 0,
        independent: 0,
        batches: 0,
        remaining: 0,
        complete: true,
    };
    const inputs = selectMapMemoryInputs(args.db, args.projectIdentity, args.sessionDirectory);
    if (inputs.length === 0) return result;

    const batches: MapMemoryInput[][] = [];
    for (let i = 0; i < inputs.length; i += MAP_BATCH_SIZE) {
        batches.push(inputs.slice(i, i + MAP_BATCH_SIZE));
    }
    result.remaining = inputs.length;

    const abortController = new AbortController();
    const heartbeat = startLeaseHeartbeat(
        args.db,
        args.holderId,
        args.leaseKey,
        () => abortController.abort(),
        args.leaseAcquisition,
    );

    try {
        let consecutiveTimeouts = 0;
        let timeoutStreakElapsedMs: number[] = [];
        for (let i = 0; i < batches.length; i += 1) {
            const remainingMs = Math.max(0, args.deadline - Date.now());
            if (remainingMs <= 0) {
                result.stopReason = "deadline";
                break;
            }
            // Do not start a batch that cannot receive a fair agentic slice. Its
            // mappings are host-committed, so stopping here preserves prior batches
            // and lets a later run continue with this untouched remainder.
            if (remainingMs < MAP_BATCH_FLOOR_MS) {
                result.stopReason = "deadline";
                log(
                    `[dreamer] map-memories: stopping before batch ${i + 1}/${batches.length} — remaining budget ${remainingMs}ms is below the ${MAP_BATCH_FLOOR_MS}ms batch floor; banking ${result.mapped + result.independent} mapping(s)`,
                );
                break;
            }
            const sliceMs = computeMapBatchSliceMs(remainingMs, batches.length - i);
            const batch = batches[i];
            if (!batch) break;
            const outcome = await mapOneBatch(args, batch, sliceMs, abortController.signal);
            const committed = outcome.mapped + outcome.independent;
            result.mapped += outcome.mapped;
            result.independent += outcome.independent;
            result.remaining -= committed;
            if (committed > 0) {
                result.batches += 1;
                args.onProgress?.(result.mapped + result.independent);
            }

            if (outcome.failure?.class === "timeout") {
                consecutiveTimeouts += 1;
                timeoutStreakElapsedMs.push(outcome.failure.elapsedMs);
                if (consecutiveTimeouts >= CONSECUTIVE_TIMEOUT_LIMIT) {
                    result.stopReason = "timeout-circuit-breaker";
                    log(
                        `[dreamer] map-memories starvation: circuit breaker tripped after ${consecutiveTimeouts} consecutive batch timeouts (model too slow for its time slice); per-batch elapsed [${timeoutStreakElapsedMs.join("ms, ")}ms] vs ${sliceMs}ms slice; stopping with ${result.remaining} mapping(s) remaining`,
                    );
                    break;
                }
            } else {
                // A success or non-timeout failure should not poison the next
                // timeout streak; those preserve the existing retry-next-run path.
                consecutiveTimeouts = 0;
                timeoutStreakElapsedMs = [];
            }
        }
        result.complete = result.remaining === 0;
        log(
            `[dreamer] map-memories: mapped=${result.mapped} independent=${result.independent} batches=${result.batches} remaining=${result.remaining} complete=${result.complete}${result.stopReason ? ` stop_reason=${result.stopReason}` : ""}`,
        );
        return result;
    } finally {
        heartbeat.stop();
    }
}

/**
 * Map ONE batch in its OWN child session. Per-batch try/finally guarantees the
 * child is deleted even on a mid-loop deadline throw. A batch that fails or emits
 * no manifest records nothing (its memories stay unmapped for the next run) —
 * never yields a partial-wrong mapping.
 */
async function mapOneBatch(
    args: MapMemoriesArgs,
    batch: MapMemoryInput[],
    sliceMs: number,
    signal: AbortSignal,
): Promise<MapBatchOutcome> {
    let agentSessionId: string | null = null;
    const startedAt = Date.now();
    try {
        const createResponse = await createChildSessionWithFence({
            client: args.client,
            db: args.db,
            parentSessionId: args.parentSessionId,
            title: "magic-context-dream-map-memories",
            directory: args.sessionDirectory,
        });
        const created = shared.normalizeSDKResponse(
            createResponse,
            null as { id?: string } | null,
            {
                preferResponseOnMissingData: true,
            },
        );
        agentSessionId = typeof created?.id === "string" ? created.id : null;
        if (!agentSessionId) throw new Error("Could not create map-memories session.");

        const prompt = buildMapMemoriesPrompt(args.projectIdentity, batch);
        const run = await shared.promptSyncWithValidatedOutputRetry(
            args.client,
            {
                path: { id: agentSessionId },
                query: { directory: args.sessionDirectory },
                body: {
                    agent: DREAMER_MEMORY_MAPPER_AGENT,
                    system: MAP_MEMORIES_SYSTEM_PROMPT,
                    ...modelBodyField(args.model),
                    parts: [{ type: "text", text: prompt, synthetic: true }],
                },
            },
            {
                timeoutMs: sliceMs,
                signal,
                fallbackModels: args.fallbackModels,
                callContext: "dreamer:map-memories",
                fetchOutput: async () => {
                    const messagesResponse = await args.client.session.messages({
                        path: { id: agentSessionId as string },
                        query: { directory: args.sessionDirectory, limit: 100 },
                    });
                    return shared.normalizeSDKResponse(messagesResponse, [] as unknown[], {
                        preferResponseOnMissingData: true,
                    });
                },
                validateOutput: (messages) => {
                    if (hasLengthCappedOutput(messages)) {
                        throw new Error("map-memories returned length-capped output");
                    }
                    const text = extractLatestAssistantText(messages);
                    if (!text) throw new Error("map-memories returned no output");
                    validateMapMemoriesManifest(text, new Set(batch.map((memory) => memory.id)));
                    return text;
                },
            },
        );

        recordInvocation(args, startedAt, { status: "completed", messages: run.output });
        return await applyBatchMappings(args, batch, run.validated);
    } catch (error) {
        const desc = describeError(error);
        log(
            `[dreamer] map-memories batch failed: ${desc.brief}`,
            desc.stackHead ? { stackHead: desc.stackHead } : undefined,
        );
        recordInvocation(args, startedAt, { status: "failed", error });
        if (error instanceof DreamerModuleFailureError) throw error;
        // Swallow per-batch failures: the batch's memories stay unmapped and are
        // retried next run. Only an abort/lease-loss should stop the whole task.
        if (signal.aborted) throw error;
        return {
            mapped: 0,
            independent: 0,
            failure: {
                class: isTimeoutClassError(error) ? "timeout" : "other",
                elapsedMs: Date.now() - startedAt,
            },
        };
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
                    log(`[dreamer] map-memories session cleanup failed: ${getErrorMessage(e)}`);
                });
        }
    }
}

/** Parse the complete manifest, normalize paths the same way verify does, and
 *  write the mappings under one lease-guarded transaction. The manifest must
 *  cover exactly this batch; unknown or missing ids reject the whole batch. */
export async function applyBatchMappings(
    args: MapMemoriesArgs,
    batch: MapMemoryInput[],
    manifestText: string,
): Promise<{ mapped: number; independent: number }> {
    const batchIds = new Set(batch.map((m) => m.id));
    const parsed = parseMapMemoriesManifest(manifestText);
    assertManifestCoversExactly(
        parsed.map((entry) => entry.id),
        batchIds,
        "mappings",
    );
    if (parsed.length === 0) return { mapped: 0, independent: 0 };

    // Pre-normalize each mapping's files OUTSIDE the transaction (path
    // normalization does git/realpath I/O). Independent → sentinel (empty set).
    const planned: Array<{ id: number; files: string[]; independent: boolean }> = [];
    for (const p of parsed) {
        if (p.independent) {
            planned.push({ id: p.id, files: [], independent: true });
            continue;
        }
        // The parser guarantees independent XOR files-present; a files-empty entry
        // reaching here means that invariant broke upstream. Refuse rather than
        // default to independent — a silent independent=true removes the memory
        // from the verify gate permanently (the #323 corruption shape).
        if (p.files.length === 0) {
            throw new Error(`mapping entry ${p.id} has no files and no independent sentinel`);
        }
        const normalized = await normalizeVerificationFiles({
            cwd: args.sessionDirectory,
            files: p.files,
        });
        // Drop a mapping whose paths all failed the existence/escape guard rather
        // than writing a wrong (empty) one as if it were file-independent.
        if (normalized.files.length === 0) continue;
        planned.push({ id: p.id, files: normalized.files, independent: false });
    }
    if (planned.length === 0) return { mapped: 0, independent: 0 };

    let mapped = 0;
    let independent = 0;
    if (args.moduleRoute) {
        const identities = getModuleMemoryIdentities(
            args.db,
            args.projectIdentity,
            planned.map((item) => item.id),
        );
        const rows = planned.map((item) => {
            const identity = identities.get(item.id);
            if (!identity)
                throw new DreamerModuleFailureError(
                    "memory.set_mapping",
                    new Error(`missing mirror identity for ${item.id}`),
                );
            return {
                memory_id: identity.moduleId,
                content_hash_at_prompt: identity.normalizedHash,
                mapped_files: item.independent ? null : item.files,
            };
        });
        let response: unknown;
        try {
            response = await args.moduleRoute.moduleClient.call({
                sessionId: args.moduleRoute.moduleSessionId,
                projectRoot: args.moduleRoute.moduleProjectRoot,
                method: "memory.set_mapping",
                body: {
                    name: "memory.set_mapping",
                    arguments: {
                        memory_project: args.projectIdentity,
                        context_store_uuid: args.moduleRoute.moduleContextStoreUuid,
                        authority_generation: args.moduleRoute.moduleAuthorityGeneration,
                        command_id: `${args.moduleRoute.moduleCommandId}:${createHash("sha256")
                            .update(rows.map((row) => row.memory_id).join(","))
                            .digest("hex")
                            .slice(0, 16)}`,
                        rows,
                    },
                },
            });
        } catch (error) {
            throw new DreamerModuleFailureError("memory.set_mapping", error);
        }
        const result = ((response as { result?: unknown })?.result ?? response) as {
            accepted?: unknown;
        };
        if (!Array.isArray(result?.accepted))
            throw new DreamerModuleFailureError(
                "memory.set_mapping",
                new Error("invalid response"),
            );
        const accepted = new Set(
            result.accepted.filter((id): id is number => typeof id === "number"),
        );
        for (const item of planned) {
            const identity = identities.get(item.id);
            if (identity && accepted.has(identity.moduleId))
                item.independent ? (independent += 1) : (mapped += 1);
        }
        return { mapped, independent };
    }
    const now = Date.now();
    runLeaseGuardedWrite(args.db, args.holderId, args.leaseKey, () => {
        for (const item of planned) {
            recordMemoryMapping(args.db, item.id, item.files, now);
            item.independent ? (independent += 1) : (mapped += 1);
        }
    });
    return { mapped, independent };
}

function recordInvocation(
    args: MapMemoriesArgs,
    startedAt: number,
    params: { status: "completed" | "failed"; messages?: unknown[]; error?: unknown },
): void {
    if (!args.parentSessionId) return;
    recordChildInvocation({
        db: args.db,
        parentSessionId: args.parentSessionId,
        harness: "opencode",
        subagent: "dreamer",
        task: "map-memories",
        startedAt,
        status: params.status,
        messages: params.messages,
        error: params.error,
    });
}
