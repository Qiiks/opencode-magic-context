/**
 * Compaction-off mode transitions (issue #266, slice S3).
 *
 * The mode is boot-resolved and process-stable, so a transition is only ever
 * observed on a session's first transform pass after a restart that changed
 * the resolved value. Each session reconciles its durable per-session mode
 * record (`session_meta.compaction_mode_record`: NULL = no record, "on",
 * "off") against the resolved mode:
 *
 *   - NULL resolves to "on" (every pre-feature session ran with compaction
 *     enabled), so `no record + configured-off` IS the off-transition — the
 *     upgrade path that guarantees marker cleanup reaches legacy sessions.
 *   - `no record + on` → write "on", no transition work.
 *   - `no record | on  → off` → exactly ONE off-transition per session:
 *       delete MC-owned compaction-marker lineages (canonical + supported
 *       legacy), clear the marker bookkeeping that references those rows,
 *       clear the emergency-recovery latch, clear any persisted Channel-2
 *       pending/claimed intent, clear pre-existing pending_ops, invalidate
 *       the cached m[0]/m[1] baseline (so the off-mode render never replays
 *       an on-mode `<session-history>`), then write "off".
 *   - `off → on` → exactly ONE on-transition: invalidate the cached
 *       m[0]/m[1] baseline (so the dormant compartments' session-history
 *       re-renders before raw-tail trimming resumes), write the historian
 *       catch-up signal (compartmentInProgress, conditioned on the historian
 *       being runnable) and offer the `/ctx-wrapup` suggestion out of band.
 *
 * Crash safety: every cleanup operation is idempotent and the record is
 * written only AFTER all transition work completes, so a crash before the
 * write re-executes the same logical transition without duplicating mutating
 * side effects. Notices are at-least-once: emitted after the work and before
 * the record commit, so the only permitted repeat is inside that crash
 * window.
 *
 * Notices are delivered OUT OF BAND by the caller (the boot-warning /
 * command-output surface) — never into the message array, never through the
 * nudge machinery. A transition pass's message-array output is
 * indistinguishable from a steady-state pass of the resolved mode.
 */

import { existsSync } from "node:fs";
import {
    getOpenCodeDbPath,
    removeMcOwnedCompactionMarkers,
    type McOwnedMarkerCleanupResult,
} from "../../features/magic-context/compaction-marker";
import { clearPendingOps, getPendingOps, updateSessionMeta } from "../../features/magic-context/storage";
import {
    clearEmergencyRecovery,
    clearPendingCompactionMarkerStateIf,
    type CompactionModeRecord,
    getChannel2NudgeState,
    getCompactionModeRecord,
    getOverflowState,
    getPendingCompactionMarkerState,
    getPersistedCompactionMarkerState,
    setChannel2NudgeState,
    setCompactionModeRecord,
    setPersistedCompactionMarkerState,
} from "../../features/magic-context/storage-meta-persisted";
import { sessionLog } from "../../shared/logger";
import { MARKER_SUMMARY_TEXT } from "./compaction-marker-manager";

let loggedUnverifiedMarkerCleanupRetry = false;

/**
 * Flip-off unfold notice. Delivered out of band on the transition pass that
 * actually cleared something. The one-cycle warning wording is contractual
 * (spec #266): removing MC's markers exposes the history hidden solely by
 * MC, and on a long session that expansion can exceed the model window once
 * before native compaction reacts. Docs quote this same constant.
 */
export const COMPACTION_OFF_FLIP_NOTICE = [
    "## Magic Context — compaction-off mode is now active",
    "",
    "Magic Context no longer manages this session's context window; native compaction (or nothing) owns it. Memory, dreamer, notes and ctx_search stay live.",
    "",
    "Magic Context's compaction markers for this session were removed, so history previously hidden by them becomes visible again — the first turn after disabling may trigger one native compaction cycle on long sessions.",
].join("\n");

/**
 * Flip-back suggestion, emitted out of band exactly once per off→on
 * transition and only when the historian is runnable (never advertising an
 * unavailable command). The gap accumulated while off is digested by the
 * normal chunked historian paths; /ctx-wrapup makes it explicit.
 */
export const COMPACTION_ON_WRAPUP_SUGGESTION = [
    "## Magic Context — compaction re-enabled",
    "",
    "Context-window management resumed for this session. History that grew while compaction was off will be picked up by the historian automatically; run `/ctx-wrapup` to digest the backlog now in bounded chunks.",
].join("\n");

export interface CompactionModeTransitionResult {
    /**
     * The record value the caller must commit AFTER emitting `notice` (the
     * at-least-once ordering). null = stored and configured values match; no
     * transition work ran and nothing must be written.
     */
    recordToWrite: CompactionModeRecord | null;
    /** Out-of-band notice text; null when the transition emits nothing. */
    notice: string | null;
    /**
     * True when the cached m[0]/m[1] baseline was invalidated. The caller
     * must drop the pass-local session-meta cached bytes too, so this pass's
     * injection re-materializes instead of replaying the pre-flip baseline.
     */
    invalidatedM0Baseline: boolean;
    /** True when the historian catch-up signal (compartmentInProgress) was written. */
    historianCatchUpSignaled: boolean;
    /** True when a stale compartmentInProgress flag was cleared (off mode). */
    clearedCompartmentInProgress: boolean;
    /** True when the off-transition cleared at least one durable MC state item. */
    clearedSomething: boolean;
    /** Marker-row cleanup detail (off-transition only; zeros otherwise). */
    markerCleanup: McOwnedMarkerCleanupResult;
}

const NO_TRANSITION: CompactionModeTransitionResult = {
    recordToWrite: null,
    notice: null,
    invalidatedM0Baseline: false,
    historianCatchUpSignaled: false,
    clearedCompartmentInProgress: false,
    clearedSomething: false,
    markerCleanup: {
        verified: true,
        removedLineages: 0,
        removedRows: 0,
        retainedLineages: 0,
    },
};

/**
 * Null the persisted m[0]/m[1] baseline bytes so the next injection pass
 * re-materializes from the mode's own render rules. The marker columns are
 * left alone: `mustMaterialize` answers `first_render` as soon as the bytes
 * are NULL, before reading any marker, and the fresh materialize overwrites
 * every marker atomically with the bytes it rendered.
 */
function clearCachedM0Baseline(
    db: import("../../shared/sqlite").Database,
    sessionId: string,
): boolean {
    const result = db
        .prepare(
            "UPDATE session_meta SET cached_m0_bytes = NULL, cached_m1_bytes = NULL, cached_m0_mural_data_url = NULL WHERE session_id = ?",
        )
        .run(sessionId);
    return (result.changes ?? 0) > 0;
}

/**
 * Reconcile one session's durable compaction-mode record against the
 * boot-resolved mode, running the transition work for a flip. Idempotent and
 * crash-safe: work first, record commit last (performed by the caller after
 * the notice is emitted). Errors propagate — the caller must skip the record
 * commit on failure so the next pass retries the same logical transition.
 */
export function reconcileCompactionMode(args: {
    db: import("../../shared/sqlite").Database;
    sessionId: string;
    /** Boot-resolved mode for this process. */
    compactionOff: boolean;
    /** False when historian.disable=true; conditions the on-transition signal. */
    historianRunnable: boolean;
    /** Pass-local session meta (drives the stale compartmentInProgress clear). */
    compartmentInProgress: boolean;
}): CompactionModeTransitionResult {
    const { db, sessionId } = args;
    const stored = getCompactionModeRecord(db, sessionId);

    if (!args.compactionOff) {
        if (stored === "on") return NO_TRANSITION;
        if (stored === null) {
            return { ...NO_TRANSITION, recordToWrite: "on" };
        }
        // stored === "off" → the on-transition.
        //
        // Invalidate the cached baseline FIRST: the off-mode baseline carries
        // no <session-history>, and raw-tail trimming resumes on flip-back.
        // Without a fresh fold the first busting pass would trim to the
        // latest dormant boundary while nothing renders the dormant range —
        // silent history loss. first_render re-cut makes the trim and the
        // rendered history agree again.
        const invalidatedM0Baseline = clearCachedM0Baseline(db, sessionId);
        let historianCatchUpSignaled = false;
        if (args.historianRunnable) {
            // Historian catch-up signal: prime the compartment phase to start
            // a catch-up run on the backlog immediately (the same flag the
            // trigger sets on fire). The trigger skips this pass because the
            // flag is already set, so exactly one start path runs.
            updateSessionMeta(db, sessionId, { compartmentInProgress: true });
            historianCatchUpSignaled = true;
        }
        return {
            ...NO_TRANSITION,
            recordToWrite: "on",
            notice: historianCatchUpSignaled ? COMPACTION_ON_WRAPUP_SUGGESTION : null,
            invalidatedM0Baseline,
            historianCatchUpSignaled,
        };
    }

    if (stored === "off") return NO_TRANSITION;

    // stored === null | "on" → the off-transition (exactly once per session).
    let clearedSomething = false;

    // 1. Delete MC-owned marker lineages from opencode.db (canonical +
    //    supported legacy). No opencode.db means no markers — not an error.
    let markerCleanup: McOwnedMarkerCleanupResult = {
        verified: true,
        removedLineages: 0,
        removedRows: 0,
        retainedLineages: 0,
    };
    if (existsSync(getOpenCodeDbPath())) {
        markerCleanup = removeMcOwnedCompactionMarkers(sessionId, MARKER_SUMMARY_TEXT);
        if (markerCleanup.removedRows > 0) clearedSomething = true;
    }

    // 2. Clear the context.db marker bookkeeping that references the deleted
    //    rows. Leaving it would dangle: the reconciler would replay a summary
    //    whose opencode.db rows are gone, and a flip-back drain would re-inject
    //    a marker at a boundary whose lineage was just removed.
    if (getPersistedCompactionMarkerState(db, sessionId) !== null) {
        setPersistedCompactionMarkerState(db, sessionId, null);
        clearedSomething = true;
    }
    const pendingMarker = getPendingCompactionMarkerState(db, sessionId);
    if (pendingMarker !== null) {
        clearPendingCompactionMarkerStateIf(db, sessionId, pendingMarker);
        clearedSomething = true;
    }

    // 3. Clear the emergency-recovery latch: a persisted
    //    needs_emergency_recovery surviving a flip-off is cleared, never
    //    honored (the whole overflow/emergency machinery is gated off).
    if (getOverflowState(db, sessionId).needsEmergencyRecovery) {
        clearEmergencyRecovery(db, sessionId);
        clearedSomething = true;
    }

    // 4. Clear persisted Channel-2 pending/claimed intent. The terminal
    //    "delivered" cap stays — the single ceiling nudge remains consumed.
    const channel2State = getChannel2NudgeState(db, sessionId);
    if (channel2State === "pending" || channel2State === "claimed") {
        setChannel2NudgeState(db, sessionId, "");
        clearedSomething = true;
    }

    // 5. Clear pre-existing pending_ops so queued drop intents cannot survive
    //    dormant and apply on flip-back.
    if (getPendingOps(db, sessionId).length > 0) {
        clearPendingOps(db, sessionId);
        clearedSomething = true;
    }

    // 6. Invalidate the cached m[0]/m[1] baseline: the on-mode bytes carry a
    //    <session-history> render, which the off mode must never replay even
    //    though historical compartment rows exist. Not counted toward the
    //    notice gate — the spec's "cleared something" list is the MC-state
    //    items above.
    const invalidatedM0Baseline = clearCachedM0Baseline(db, sessionId);

    // 7. A stale compartmentInProgress flag (a historian run that crashed
    //    before the flip) can never be consumed in off mode; clear it so the
    //    session state is honest and flip-back starts clean.
    let clearedCompartmentInProgress = false;
    if (args.compartmentInProgress) {
        updateSessionMeta(db, sessionId, { compartmentInProgress: false });
        clearedCompartmentInProgress = true;
    }

    sessionLog(
        sessionId,
        `compaction-off transition: marker cleanup verified=${markerCleanup.verified}, removed=${markerCleanup.removedLineages} lineage(s)/${markerCleanup.removedRows} row(s), retained=${markerCleanup.retainedLineages}, clearedSomething=${clearedSomething}`,
    );
    if (!markerCleanup.verified && !loggedUnverifiedMarkerCleanupRetry) {
        loggedUnverifiedMarkerCleanupRetry = true;
        sessionLog(
            sessionId,
            "compaction-off transition could not verify complete marker cleanup; the mode record remains uncommitted and cleanup will retry on the next pass",
        );
    }

    return {
        recordToWrite: markerCleanup.verified ? "off" : null,
        notice: clearedSomething ? COMPACTION_OFF_FLIP_NOTICE : null,
        invalidatedM0Baseline,
        historianCatchUpSignaled: false,
        clearedCompartmentInProgress,
        clearedSomething,
        markerCleanup,
    };
}

/**
 * Commit the mode record AFTER transition work + notice emission. Kept
 * separate so the caller controls the at-least-once notice ordering.
 */
export function commitCompactionModeRecord(
    db: import("../../shared/sqlite").Database,
    sessionId: string,
    record: CompactionModeRecord,
): void {
    setCompactionModeRecord(db, sessionId, record);
}
