import type { ContextDatabase } from "@magic-context/core/features/magic-context/storage";
import {
	clearCachedM0M1,
	clearPendingOps,
	getOrCreateSessionMeta,
	getPendingOps,
	getPendingPiCompactionMarkerState,
	setChannel2NudgeState,
	setPendingPiCompactionMarkerState,
	updateSessionMeta,
} from "@magic-context/core/features/magic-context/storage";
import {
	type CompactionModeRecord,
	clearEmergencyRecovery,
	getChannel2NudgeState,
	getCompactionModeRecord,
	getOverflowState,
	setCompactionModeRecord,
} from "@magic-context/core/features/magic-context/storage-meta-persisted";
import { COMPACTION_ENABLED_PATH } from "@magic-context/core/config/agent-disable";
import { sessionLog } from "@magic-context/core/shared/logger";

/** Exact refusal text shared by Pi's context-management commands. */
export const COMPACTION_OFF_COMMAND_UNAVAILABLE = `Unavailable: magic-context is in compaction-off mode (${COMPACTION_ENABLED_PATH}=false).`;

/**
 * Pi has no OpenCode marker rows to delete. Its MC-owned compaction state is the
 * pending JSONL marker payload plus the in-process deferred-drain signals; the
 * latter are cleared by the context handler after this durable cleanup succeeds.
 */
export interface PiCompactionModeTransition {
	recordToWrite: CompactionModeRecord | null;
	invalidatedBaseline: boolean;
	clearDeferredMarkerState: boolean;
	historianCatchUpSignaled: boolean;
	notice: string | null;
}

const NO_TRANSITION: PiCompactionModeTransition = {
	recordToWrite: null,
	invalidatedBaseline: false,
	clearDeferredMarkerState: false,
	historianCatchUpSignaled: false,
	notice: null,
};

const PI_COMPACTION_OFF_NOTICE =
	"Magic Context is in compaction-off mode. Native Pi compaction now owns this session's context window; the first turn after disabling may trigger one native compaction cycle on long sessions.";

const PI_COMPACTION_ON_NOTICE =
	"Magic Context context-window management resumed. Run /ctx-wrapup to digest history accumulated while compaction was off.";

/**
 * Reconcile Pi's durable mode record. Work is idempotent and the caller commits
 * `recordToWrite` only after delivering the out-of-band notice, preserving the
 * retry semantics used by the OpenCode transition path.
 */
export function reconcilePiCompactionMode(args: {
	db: ContextDatabase;
	sessionId: string;
	compactionOff: boolean;
	historianRunnable: boolean;
}): PiCompactionModeTransition {
	const stored = getCompactionModeRecord(args.db, args.sessionId);
	if (!args.compactionOff) {
		if (stored === "on") return NO_TRANSITION;
		if (stored === null) return { ...NO_TRANSITION, recordToWrite: "on" };

		clearCachedM0M1(args.db, args.sessionId);
		if (args.historianRunnable) {
			updateSessionMeta(args.db, args.sessionId, {
				compartmentInProgress: true,
			});
		}
		return {
			recordToWrite: "on",
			invalidatedBaseline: true,
			clearDeferredMarkerState: false,
			historianCatchUpSignaled: args.historianRunnable,
			notice: args.historianRunnable ? PI_COMPACTION_ON_NOTICE : null,
		};
	}

	if (stored === "off") return NO_TRANSITION;

	let clearedSomething = false;
	if (getPendingPiCompactionMarkerState(args.db, args.sessionId) !== null) {
		setPendingPiCompactionMarkerState(args.db, args.sessionId, null);
		clearedSomething = true;
	}
	if (getOverflowState(args.db, args.sessionId).needsEmergencyRecovery) {
		clearEmergencyRecovery(args.db, args.sessionId);
		clearedSomething = true;
	}
	const channel2 = getChannel2NudgeState(args.db, args.sessionId);
	if (channel2 === "pending" || channel2 === "claimed") {
		setChannel2NudgeState(args.db, args.sessionId, "");
		clearedSomething = true;
	}
	if (getPendingOps(args.db, args.sessionId).length > 0) {
		clearPendingOps(args.db, args.sessionId);
		clearedSomething = true;
	}
	const meta = getOrCreateSessionMeta(args.db, args.sessionId);
	if (meta.compartmentInProgress) {
		updateSessionMeta(args.db, args.sessionId, {
			compartmentInProgress: false,
		});
	}
	clearCachedM0M1(args.db, args.sessionId);
	sessionLog(
		args.sessionId,
		`Pi compaction-off transition: clearedPendingMarker=${clearedSomething}`,
	);
	return {
		recordToWrite: "off",
		invalidatedBaseline: true,
		clearDeferredMarkerState: true,
		historianCatchUpSignaled: false,
		notice: clearedSomething ? PI_COMPACTION_OFF_NOTICE : null,
	};
}

export function commitPiCompactionModeRecord(
	db: ContextDatabase,
	sessionId: string,
	record: CompactionModeRecord,
): void {
	setCompactionModeRecord(db, sessionId, record);
}
