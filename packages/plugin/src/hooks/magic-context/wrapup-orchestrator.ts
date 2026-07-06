import * as crypto from "node:crypto";
import {
    acquireCompartmentLease,
    COMPARTMENT_LEASE_RENEWAL_MS,
    releaseCompartmentLease,
    renewCompartmentLease,
} from "../../features/magic-context/compartment-lease";
import {
    getCompartments,
    getLastCompartmentEndMessage,
} from "../../features/magic-context/compartment-storage";
import {
    acquireWrapupInProgress,
    clearEmergencyRecovery,
    getHistorianFailureState,
    getWrapupInProgressState,
    releaseWrapupInProgress,
    updateWrapupInProgress,
} from "../../features/magic-context/storage";
import { sessionLog } from "../../shared/logger";
import {
    getActiveCompartmentRun,
    markActiveCompartmentRunPublished,
    registerActiveCompartmentRun,
} from "./compartment-runner";
import { runCompartmentAgent } from "./compartment-runner-incremental";
import type { RecompProgress } from "./compartment-runner-types";
import {
    hasRunnableCompartmentWindow,
    resolveWrapupProtectedTailBoundary,
    type WrapupBoundaryPlan,
} from "./protected-tail-boundary";
import type { ManagedRecompContext } from "./recomp-orchestrator";
import { setRecompStarting, setRecompTerminal } from "./recomp-orchestrator";

export interface ManagedWrapupContext extends ManagedRecompContext {
    contextLimit: number;
    executeThresholdPercentage: number;
    hasPendingNaturalBust?: (sessionId: string) => boolean;
    runCompartmentAgentForWrapup?: typeof runCompartmentAgent;
}

export interface WrapupOptions {
    messagesToKeep: number;
}

const WAIT_FOR_LEASE_MS = 1_000;
type WrapupProgressUpdate = Parameters<typeof updateWrapupInProgress>[3];

function sleep(ms: number): Promise<void> {
    return new Promise((resolve) => setTimeout(resolve, ms));
}

function plural(value: number, word: string): string {
    return `${value} ${word}${value === 1 ? "" : "s"}`;
}

function formatAlreadyRunningMessage(state: ReturnType<typeof getWrapupInProgressState>): string {
    if (!state) return "/ctx-wrapup is already running for this session.";
    const chunk =
        state.expectedChunks > 0 ? ` chunk ${state.chunkIndex}/${state.expectedChunks}` : "";
    const through =
        state.lastCompartmentEnd > 0 ? ` through message ${state.lastCompartmentEnd}` : "";
    return `/ctx-wrapup is already running for this session${chunk}${through}. Wait for it to finish, then run /ctx-wrapup again if more history remains.`;
}

function appendFlushHint(ctx: ManagedWrapupContext, sessionId: string, message: string): string {
    if (ctx.hasPendingNaturalBust?.(sessionId)) return message;
    return `${message} If you want it applied on the very next message, run /ctx-flush first.`;
}

function buildPlan(
    ctx: ManagedWrapupContext,
    sessionId: string,
    messagesToKeep: number,
    anchorRawMessageCount?: number,
): WrapupBoundaryPlan {
    return resolveWrapupProtectedTailBoundary({
        db: ctx.db,
        sessionId,
        mode: "manual-wrapup",
        contextLimit: ctx.contextLimit,
        executeThresholdPercentage: ctx.executeThresholdPercentage,
        usage: null,
        usageSource: "manual-none",
        providerShapeVersion: "opencode-v1",
        cacheNamespace: `opencode:${sessionId}`,
        messagesToKeep,
        anchorRawMessageCount,
    });
}

function emitWrapupProgress(
    ctx: ManagedWrapupContext,
    sessionId: string,
    progress: Partial<RecompProgress> & Pick<RecompProgress, "note">,
): void {
    const current = ctx.liveSessionState.recompProgressBySession.get(sessionId);
    ctx.liveSessionState.recompProgressBySession.set(sessionId, {
        sessionId,
        kind: "wrapup",
        phase: "recomp",
        processedMessages: progress.processedMessages ?? current?.processedMessages ?? 0,
        totalMessages: progress.totalMessages ?? current?.totalMessages ?? 0,
        passCount: progress.passCount ?? current?.passCount ?? 0,
        compartmentsCreated: progress.compartmentsCreated ?? current?.compartmentsCreated ?? 0,
        startedAt: current?.startedAt ?? Date.now(),
        updatedAt: Date.now(),
        note: progress.note,
    });
}

async function waitForExistingIncrementalRun(sessionId: string): Promise<"ok" | "busy"> {
    const active = getActiveCompartmentRun(sessionId);
    if (!active) return "ok";
    if (active.kind === "recomp" || active.kind === "wrapup") return "busy";
    try {
        await active.promise;
    } catch {
        // The active runner records its own failure state; wrapup just resumes from
        // the durable compartment boundary after it settles.
    }
    return "ok";
}

async function acquireCompartmentLeaseForWrapup(
    ctx: ManagedWrapupContext,
    sessionId: string,
    renewWrapupMarker: (updates: WrapupProgressUpdate) => boolean,
): Promise<string | null> {
    const holderId = crypto.randomUUID();
    for (;;) {
        const active = getActiveCompartmentRun(sessionId);
        if (active?.kind === "recomp" || active?.kind === "wrapup") return null;
        if (active) {
            emitWrapupProgress(ctx, sessionId, { note: "Waiting for the active historian run…" });
            try {
                await active.promise;
            } catch {
                // Existing run owns its failure reporting.
            }
            continue;
        }
        const lease = acquireCompartmentLease(ctx.db, sessionId, holderId);
        if (lease) return holderId;
        emitWrapupProgress(ctx, sessionId, { note: "Waiting for the compartment-state lease…" });
        if (!renewWrapupMarker({})) return null;
        await sleep(WAIT_FOR_LEASE_MS);
    }
}

async function runOneWrapupIteration(args: {
    ctx: ManagedWrapupContext;
    sessionId: string;
    plan: WrapupBoundaryPlan;
    messagesToKeep: number;
    anchorRawMessageCount: number;
    renewWrapupMarker: (updates: WrapupProgressUpdate) => boolean;
}): Promise<boolean> {
    const { ctx, sessionId, plan, messagesToKeep, anchorRawMessageCount } = args;
    const leaseHolderId = await acquireCompartmentLeaseForWrapup(
        ctx,
        sessionId,
        args.renewWrapupMarker,
    );
    if (!leaseHolderId) return false;
    const renewal = setInterval(() => {
        if (!renewCompartmentLease(ctx.db, sessionId, leaseHolderId)) {
            sessionLog(sessionId, "wrapup: compartment lease renewal failed");
        }
    }, COMPARTMENT_LEASE_RENEWAL_MS);
    const runCompartmentAgentForWrapup = ctx.runCompartmentAgentForWrapup ?? runCompartmentAgent;
    const runnerPromise = runCompartmentAgentForWrapup({
        client: ctx.client,
        db: ctx.db,
        sessionId,
        historianChunkTokens: ctx.historianChunkTokens,
        historianTimeoutMs: ctx.historianTimeoutMs,
        boundarySnapshot: plan.snapshot,
        currentContextLimit: ctx.contextLimit,
        directory: ctx.directory,
        fallbackModels: ctx.fallbackModels,
        fallbackModelId: ctx.fallbackModelId,
        language: ctx.language,
        historianTwoPass: ctx.historianTwoPass,
        memoryEnabled: ctx.memoryEnabled,
        autoPromote: ctx.autoPromote,
        ensureProjectRegistered: ctx.ensureProjectRegistered,
        getNotificationParams: () => ctx.getNotificationParams(sessionId),
        preserveInjectionCacheUntilConsumed: true,
        compartmentLeaseHolderId: leaseHolderId,
        forceDrainQuota: true,
        // Wrapup wants coverage on the actual final chunk. The runner downgrades
        // this hint whenever readSessionChunk reports more raw history remains.
        forceKeepLastCompartment: true,
        refreshBoundarySnapshot: () =>
            buildPlan(ctx, sessionId, messagesToKeep, anchorRawMessageCount).snapshot,
        onCompartmentStatePublished: (sid) => {
            markActiveCompartmentRunPublished(sid);
            ctx.liveSessionState.deferredHistoryRefreshSessions.add(sid);
            ctx.liveSessionState.deferredMaterializationSessions.add(sid);
        },
        onDeferredMarkerPending: (sid) => {
            ctx.liveSessionState.deferredHistoryRefreshSessions.add(sid);
        },
    });
    registerActiveCompartmentRun(sessionId, runnerPromise, "wrapup");
    try {
        await runnerPromise;
        return true;
    } finally {
        clearInterval(renewal);
        releaseCompartmentLease(ctx.db, sessionId, leaseHolderId);
    }
}

export async function runManagedWrapup(
    ctx: ManagedWrapupContext,
    sessionId: string,
    options: WrapupOptions,
): Promise<string> {
    const messagesToKeep = Math.max(1, Math.floor(options.messagesToKeep));
    setRecompStarting(ctx.liveSessionState, sessionId, "Estimating wrapup…", "wrapup");

    const existingWrapup = getWrapupInProgressState(ctx.db, sessionId);
    if (existingWrapup) {
        const message = formatAlreadyRunningMessage(existingWrapup);
        setRecompTerminal(ctx.liveSessionState, sessionId, "skipped", message);
        return `## Magic Wrapup — Skipped\n\n${message}`;
    }

    const initialPlan = buildPlan(ctx, sessionId, messagesToKeep);
    if (
        initialPlan.rawMessagesAboveLastCompartment <= messagesToKeep ||
        !hasRunnableCompartmentWindow(initialPlan.snapshot)
    ) {
        const message = `Nothing to wrap up — only ${initialPlan.rawMessagesAboveLastCompartment} messages above the last compartment.`;
        setRecompTerminal(ctx.liveSessionState, sessionId, "done", message);
        return message;
    }

    const expectedChunks = Math.max(
        1,
        Math.ceil(
            initialPlan.snapshot.trueRawEligibleTokens / Math.max(1, ctx.historianChunkTokens),
        ),
    );
    const wrapupHolderId = crypto.randomUUID();
    const acquired = acquireWrapupInProgress(ctx.db, sessionId, {
        holderId: wrapupHolderId,
        messagesToKeep,
        anchorRawMessageCount: initialPlan.anchorRawMessageCount,
        targetEligibleEndOrdinal: initialPlan.targetEligibleEndOrdinal,
        lastCompartmentEnd: getLastCompartmentEndMessage(ctx.db, sessionId),
        chunkIndex: 0,
        expectedChunks,
    });
    if (!acquired.ok) {
        const message = formatAlreadyRunningMessage(acquired.state);
        setRecompTerminal(ctx.liveSessionState, sessionId, "skipped", message);
        return `## Magic Wrapup — Skipped\n\n${message}`;
    }

    const startLastEnd = getLastCompartmentEndMessage(ctx.db, sessionId);
    const startCompartmentCount = getCompartments(ctx.db, sessionId).length;
    let chunkIndex = 0;
    let lastEnd = startLastEnd;
    let stoppedForFailure = false;
    let stoppedReason = "";
    let ownershipLost = false;
    const ownershipLostReason = "another process took over this session's wrapup.";
    const markOwnershipLost = (): void => {
        if (ownershipLost) return;
        ownershipLost = true;
        sessionLog(sessionId, "wrapup: durable marker ownership lost; aborting loop");
    };
    const renewWrapupMarker = (updates: WrapupProgressUpdate): boolean => {
        const updated = updateWrapupInProgress(ctx.db, sessionId, wrapupHolderId, updates);
        if (!updated) {
            markOwnershipLost();
            return false;
        }
        return true;
    };
    const markerRenewal = setInterval(() => {
        renewWrapupMarker({
            lastCompartmentEnd: getLastCompartmentEndMessage(ctx.db, sessionId),
            chunkIndex,
        });
    }, 60_000);
    (markerRenewal as { unref?: () => void }).unref?.();

    try {
        const activeAtStart = await waitForExistingIncrementalRun(sessionId);
        if (activeAtStart === "busy") {
            const message = "Another Magic Context rebuild is already running for this session.";
            setRecompTerminal(ctx.liveSessionState, sessionId, "skipped", message);
            return `## Magic Wrapup — Skipped\n\n${message}`;
        }

        if (ownershipLost) {
            stoppedForFailure = true;
            stoppedReason = ownershipLostReason;
        } else {
            emitWrapupProgress(ctx, sessionId, {
                processedMessages: Math.max(0, lastEnd),
                totalMessages: Math.max(0, initialPlan.targetEligibleEndOrdinal - 1),
                passCount: 0,
                compartmentsCreated: 0,
                note: `Eligible ${plural(initialPlan.snapshot.trueRawEligibleTokens, "token")} across about ${plural(expectedChunks, "chunk")}.`,
            });

            for (;;) {
                if (ownershipLost) {
                    stoppedForFailure = true;
                    stoppedReason = ownershipLostReason;
                    break;
                }
                if (
                    !renewWrapupMarker({
                        lastCompartmentEnd: getLastCompartmentEndMessage(ctx.db, sessionId),
                        chunkIndex,
                        expectedChunks,
                    })
                ) {
                    stoppedForFailure = true;
                    stoppedReason = ownershipLostReason;
                    break;
                }

                const plan = buildPlan(
                    ctx,
                    sessionId,
                    messagesToKeep,
                    initialPlan.anchorRawMessageCount,
                );
                lastEnd = getLastCompartmentEndMessage(ctx.db, sessionId);
                if (lastEnd + 1 >= plan.targetEligibleEndOrdinal) break;

                chunkIndex += 1;
                emitWrapupProgress(ctx, sessionId, {
                    processedMessages: Math.max(0, lastEnd),
                    totalMessages: Math.max(0, plan.targetEligibleEndOrdinal - 1),
                    passCount: chunkIndex - 1,
                    compartmentsCreated: Math.max(
                        0,
                        getCompartments(ctx.db, sessionId).length - startCompartmentCount,
                    ),
                    note: `Chunk ${chunkIndex}/${expectedChunks}: messages ${plan.snapshot.offset}-${plan.snapshot.eligibleEndOrdinal - 1}…`,
                });
                if (
                    !renewWrapupMarker({
                        lastCompartmentEnd: lastEnd,
                        chunkIndex,
                        expectedChunks,
                        targetEligibleEndOrdinal: plan.targetEligibleEndOrdinal,
                    })
                ) {
                    stoppedForFailure = true;
                    stoppedReason = ownershipLostReason;
                    break;
                }

                const beforeEnd = lastEnd;
                const beforeFailures = getHistorianFailureState(ctx.db, sessionId).failureCount;
                const ran = await runOneWrapupIteration({
                    ctx,
                    sessionId,
                    plan,
                    messagesToKeep,
                    anchorRawMessageCount: initialPlan.anchorRawMessageCount,
                    renewWrapupMarker,
                });
                if (!ran) {
                    stoppedForFailure = true;
                    stoppedReason = ownershipLost
                        ? ownershipLostReason
                        : "Another Magic Context rebuild started while wrapup was waiting.";
                    break;
                }
                const afterEnd = getLastCompartmentEndMessage(ctx.db, sessionId);
                const afterFailures = getHistorianFailureState(ctx.db, sessionId).failureCount;
                if (afterEnd <= beforeEnd) {
                    stoppedForFailure = true;
                    stoppedReason =
                        afterFailures > beforeFailures
                            ? "The historian failed on the current chunk."
                            : "The historian made no forward progress on the current chunk.";
                    break;
                }
                lastEnd = afterEnd;
                emitWrapupProgress(ctx, sessionId, {
                    processedMessages: Math.max(0, lastEnd),
                    totalMessages: Math.max(0, plan.targetEligibleEndOrdinal - 1),
                    passCount: chunkIndex,
                    compartmentsCreated: Math.max(
                        0,
                        getCompartments(ctx.db, sessionId).length - startCompartmentCount,
                    ),
                    note: `Wrapped through message ${lastEnd}.`,
                });
            }
        }
    } finally {
        clearInterval(markerRenewal);
        releaseWrapupInProgress(ctx.db, sessionId, wrapupHolderId);
    }

    const finalEnd = getLastCompartmentEndMessage(ctx.db, sessionId);
    const compartmentsCreated = Math.max(
        0,
        getCompartments(ctx.db, sessionId).length - startCompartmentCount,
    );
    const messagesWrapped = Math.max(0, finalEnd - Math.max(0, startLastEnd));

    if (stoppedForFailure) {
        const message = `Wrapped up through message ${Math.max(0, finalEnd)} (${plural(messagesWrapped, "message")} into ${plural(compartmentsCreated, "compartment")}). ${stoppedReason} Run /ctx-wrapup again to continue.`;
        setRecompTerminal(ctx.liveSessionState, sessionId, "failed", message);
        return `## Magic Wrapup — Partial\n\n${message}`;
    }

    try {
        clearEmergencyRecovery(ctx.db, sessionId);
    } catch {
        // Best-effort: normal historian recovery disarm remains the backstop.
    }
    const base = `Wrapped up ${messagesWrapped} messages into ${compartmentsCreated} compartments. The compacted history is queued and materializes on your next message.`;
    const message = appendFlushHint(ctx, sessionId, base);
    setRecompTerminal(ctx.liveSessionState, sessionId, "done", message);
    return message;
}
