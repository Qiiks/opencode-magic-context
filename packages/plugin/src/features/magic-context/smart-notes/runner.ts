import type { Database } from "../../../shared/sqlite";
import { getLeaseHolder, peekLeaseHolderAndExpiry } from "../dreamer/lease";
import { leaseKeyFor } from "../dreamer/task-registry";
import { markNoteReady } from "../storage-notes";
import { createSmartNoteCapabilities } from "./capabilities";
import { runCompiledSmartNoteCheck } from "./sandbox-runner";
import { nextSmartNoteCheckDueAt } from "./schedule";
import {
    commitSmartNoteState,
    getDueCompiledSmartNoteChecks,
    markCompiledCheckFalse,
    markCompiledCheckLogicFailure,
    markCompiledCheckNetworkFailure,
} from "./storage";
import { parseSmartNoteManifest } from "./types";

export interface RunDueCompiledSmartNoteChecksArgs {
    db: Database;
    projectIdentity: string;
    projectRoot: string;
    now?: number;
    maxChecks?: number;
    sweepBudgetMs?: number;
    leaseHeld?: () => boolean;
}

export interface RunDueCompiledSmartNoteChecksResult {
    ran: number;
    surfaced: number;
    failed: number;
    networkFailed: number;
}

function inferEvaluateSmartNotesLeaseHeld(
    db: Database,
    projectIdentity: string,
): (() => boolean) | undefined {
    const leaseKey = leaseKeyFor("evaluate-smart-notes", projectIdentity);
    const holderId = getLeaseHolder(db, leaseKey);
    if (!holderId || !peekLeaseHolderAndExpiry(db, holderId, leaseKey)) return undefined;
    return () => peekLeaseHolderAndExpiry(db, holderId, leaseKey);
}

const DEFAULT_MAX_CHECKS = 10;
const DEFAULT_SWEEP_BUDGET_MS = 15_000;
const MAX_FAILURES_BEFORE_REAUTHOR = 3;

export async function runDueCompiledSmartNoteChecks(
    args: RunDueCompiledSmartNoteChecksArgs,
): Promise<RunDueCompiledSmartNoteChecksResult> {
    const startedAt = Date.now();
    const now = args.now ?? startedAt;
    const due = getDueCompiledSmartNoteChecks(
        args.db,
        args.projectIdentity,
        now,
        args.maxChecks ?? DEFAULT_MAX_CHECKS,
    );
    let ran = 0;
    let surfaced = 0;
    let failed = 0;
    let networkFailed = 0;
    const leaseHeld =
        args.leaseHeld ?? inferEvaluateSmartNotesLeaseHeld(args.db, args.projectIdentity);

    for (const note of due) {
        if (Date.now() - startedAt >= (args.sweepBudgetMs ?? DEFAULT_SWEEP_BUDGET_MS)) break;
        if (!note.compiledCheck) continue;
        ran++;
        const controller = new AbortController();
        const remaining = Math.max(
            500,
            (args.sweepBudgetMs ?? DEFAULT_SWEEP_BUDGET_MS) - (Date.now() - startedAt),
        );
        const timer = setTimeout(
            () => controller.abort(new Error("smart-note sweep budget exhausted")),
            remaining,
        );
        try {
            const result = await runCompiledSmartNoteCheck({
                compiledCheck: note.compiledCheck,
                capabilityFactory: (signal) =>
                    createSmartNoteCapabilities({
                        projectRoot: args.projectRoot,
                        signal,
                    }),
                signal: controller.signal,
                timeoutMs: Math.min(2_000, remaining),
            });
            const runFinishedAt = Date.now();
            if (result.ok && result.result.met) {
                commitSmartNoteState(args.db, {
                    phase: "due check",
                    leaseHeld,
                    write: () => {
                        markNoteReady(
                            args.db,
                            note.id,
                            hostGeneratedReadyReason(note.id, note.manifestJson),
                        );
                    },
                });
                surfaced++;
            } else if (result.ok) {
                commitSmartNoteState(args.db, {
                    phase: "due check",
                    leaseHeld,
                    write: () => {
                        markCompiledCheckFalse(
                            args.db,
                            note.id,
                            nextSmartNoteCheckDueAt(note.checkCron, {
                                now: runFinishedAt,
                                noteId: note.id,
                                hash: note.checkHash,
                            }),
                            runFinishedAt,
                        );
                    },
                });
            } else if (result.network) {
                commitSmartNoteState(args.db, {
                    phase: "network failure",
                    leaseHeld,
                    write: () => {
                        markCompiledCheckNetworkFailure(
                            args.db,
                            note.id,
                            runFinishedAt,
                            MAX_FAILURES_BEFORE_REAUTHOR,
                        );
                    },
                });
                networkFailed++;
            } else {
                commitSmartNoteState(args.db, {
                    phase: "logic failure",
                    leaseHeld,
                    write: () => {
                        markCompiledCheckLogicFailure(
                            args.db,
                            note.id,
                            runFinishedAt,
                            MAX_FAILURES_BEFORE_REAUTHOR,
                        );
                    },
                });
                failed++;
            }
        } finally {
            clearTimeout(timer);
        }
    }

    return { ran, surfaced, failed, networkFailed };
}

function hostGeneratedReadyReason(noteId: number, manifestJson: string | null): string {
    const manifest = parseSmartNoteManifest(manifestJson);
    const signal = manifest.signals?.[0] ?? manifest.summary ?? "compiled check returned met=true";
    return `Smart note #${noteId}: ${signal}`.slice(0, 240);
}
