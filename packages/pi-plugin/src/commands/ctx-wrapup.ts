import * as crypto from "node:crypto";
import type {
	ExtensionAPI,
	ExtensionCommandContext,
} from "@earendil-works/pi-coding-agent";
import {
	acquireCompartmentLease,
	COMPARTMENT_LEASE_RENEWAL_MS,
	releaseCompartmentLease,
	renewCompartmentLease,
} from "@magic-context/core/features/magic-context/compartment-lease";
import {
	getCompartments,
	getLastCompartmentEndMessage,
} from "@magic-context/core/features/magic-context/compartment-storage";
import {
	acquireWrapupInProgress,
	type ContextDatabase,
	clearEmergencyRecovery,
	clearPendingPiCompactionMarkerStateIf,
	getPendingPiCompactionMarkerState,
	getWrapupInProgressState,
	releaseWrapupInProgress,
	updateWrapupInProgress,
} from "@magic-context/core/features/magic-context/storage";
import { resolveExecuteThreshold } from "@magic-context/core/hooks/magic-context/event-resolvers";
import { resolveWrapupProtectedTailBoundary } from "@magic-context/core/hooks/magic-context/protected-tail-boundary";
import { setRawMessageProvider } from "@magic-context/core/hooks/magic-context/read-session-chunk";
import type { SubagentRunner } from "@magic-context/core/shared/subagent-runner";
import { applyDeferredPiCompactionMarker } from "../compaction-marker-manager-pi";
import {
	signalPiDeferredHistoryRefresh,
	signalPiDeferredMaterialization,
} from "../context-handler";
import { ensureProjectRegisteredFromPiDirectory } from "../embedding-bootstrap";
import { runPiHistorian } from "../pi-historian-runner";
import { isPiRecompInFlight } from "../pi-recomp-runner";
import { readPiSessionMessages } from "../read-session-pi";
import { updateStatusLine } from "../status-line";
import { resolveSessionId, sendCtxStatusMessage } from "./pi-command-utils";

export interface RegisterCtxWrapupDeps {
	db: ContextDatabase;
	runner: SubagentRunner;
	historianModel: string | undefined;
	historianChunkTokens: number;
	historianFallbacks?: readonly string[];
	historianTimeoutMs?: number;
	historianThinkingLevel?: string;
	language?: string;
	memoryEnabled: boolean;
	autoPromote: boolean;
	userMemoriesEnabled?: boolean;
	executeThresholdPercentage?:
		| number
		| { default: number; [modelKey: string]: number };
	executeThresholdTokens?: {
		default?: number;
		[modelKey: string]: number | undefined;
	};
	runPiHistorianForWrapup?: typeof runPiHistorian;
	drainPendingPiMarkerForWrapup?: typeof drainPendingPiMarker;
}

const DEFAULT_MESSAGES_TO_KEEP = 20;
const LEASE_WAIT_MS = 1_000;

export function parseWrapupArgs(
	raw: string,
): { ok: true; messagesToKeep: number } | { ok: false; message: string } {
	const trimmed = raw.trim();
	if (trimmed === "")
		return { ok: true, messagesToKeep: DEFAULT_MESSAGES_TO_KEEP };
	if (!/^\d+$/.test(trimmed)) {
		return {
			ok: false,
			message:
				"Usage: `/ctx-wrapup [messages_to_keep]` where messages_to_keep is a positive integer.",
		};
	}
	const messagesToKeep = Number.parseInt(trimmed, 10);
	if (!Number.isSafeInteger(messagesToKeep) || messagesToKeep <= 0) {
		return {
			ok: false,
			message: "messages_to_keep must be a positive integer.",
		};
	}
	return { ok: true, messagesToKeep };
}

export function registerCtxWrapupCommand(
	pi: ExtensionAPI,
	deps: RegisterCtxWrapupDeps,
): void {
	pi.registerCommand("ctx-wrapup", {
		description:
			"Compact older Magic Context history while keeping the newest messages raw",
		handler: async (args, ctx) => {
			const sessionId = resolveSessionId(ctx);
			if (!sessionId) {
				sendCtxStatusMessage(pi, {
					title: "/ctx-wrapup",
					text: "## Magic Wrapup\n\nNo active Pi session is available.",
					level: "error",
				});
				return;
			}

			if (!deps.historianModel) {
				sendCtxStatusMessage(pi, {
					title: "/ctx-wrapup",
					text: "## Magic Wrapup\n\n/ctx-wrapup is unavailable because `historian.model` is not configured.",
					level: "error",
				});
				return;
			}

			const parsed = parseWrapupArgs(args);
			if (!parsed.ok) {
				sendCtxStatusMessage(pi, {
					title: "/ctx-wrapup",
					text: `## Magic Wrapup — Invalid Arguments\n\n${parsed.message}`,
					level: "error",
				});
				return;
			}

			const result = await runPiWrapup(
				pi,
				deps,
				ctx,
				sessionId,
				parsed.messagesToKeep,
			);
			sendCtxStatusMessage(pi, {
				title: "/ctx-wrapup",
				text: result,
				level: result.includes("Failed") ? "error" : "success",
			});
		},
	});
}

export async function runPiWrapup(
	pi: ExtensionAPI,
	deps: RegisterCtxWrapupDeps,
	ctx: ExtensionCommandContext,
	sessionId: string,
	messagesToKeep: number,
): Promise<string> {
	if (isPiRecompInFlight(sessionId)) {
		return "## Magic Wrapup — Skipped\n\nA recomp or upgrade is already running for this session. Wait for it to finish, then try `/ctx-wrapup` again.";
	}

	const provider = { readMessages: () => readPiSessionMessages(ctx) };
	const unregister = setRawMessageProvider(sessionId, provider);
	let holderId = "";
	try {
		const contextLimit = resolvePiContextLimit(ctx);
		const modelKey = ctx.model
			? `${ctx.model.provider}/${ctx.model.id}`
			: undefined;
		const executeThresholdPercentage = resolveExecuteThreshold(
			deps.executeThresholdPercentage ?? 65,
			modelKey,
			65,
			{
				tokensConfig: deps.executeThresholdTokens,
				contextLimit,
				sessionId,
			},
		);
		const initialPlan = resolveWrapupProtectedTailBoundary({
			db: deps.db,
			sessionId,
			mode: "manual-wrapup",
			contextLimit,
			executeThresholdPercentage,
			usage: { percentage: 0, inputTokens: 0 },
			usageSource: "manual-none",
			providerShapeVersion: "pi-folded-v1",
			cacheNamespace: `pi:${sessionId}`,
			messagesToKeep,
		});
		if (initialPlan.meaningfulMessagesAboveLastCompartment <= messagesToKeep) {
			return `## Magic Wrapup\n\nNothing to wrap up — only ${initialPlan.meaningfulMessagesAboveLastCompartment} messages above the last compartment.`;
		}

		holderId = crypto.randomUUID();
		const acquired = acquireWrapupInProgress(deps.db, sessionId, {
			holderId,
			messagesToKeep,
			anchorRawMessageCount: initialPlan.anchorRawMessageCount,
			targetEligibleEndOrdinal: initialPlan.targetEligibleEndOrdinal,
			lastCompartmentEnd: getLastCompartmentEndMessage(deps.db, sessionId),
			chunkIndex: 0,
			expectedChunks: estimateChunks(
				initialPlan.snapshot.trueRawEligibleTokens,
				deps.historianChunkTokens,
			),
		});
		if (!acquired.ok) {
			return formatExistingWrapup(
				acquired.state ?? getWrapupInProgressState(deps.db, sessionId),
			);
		}

		let ownershipLost = false;
		const ownershipLostReason =
			"another process took over this session's wrapup";
		const markOwnershipLost = (): void => {
			if (ownershipLost) return;
			ownershipLost = true;
		};
		const renewWrapupMarker = (
			updates: Parameters<typeof updateWrapupInProgress>[3],
		): boolean => {
			const updated = updateWrapupInProgress(
				deps.db,
				sessionId,
				holderId,
				updates,
			);
			if (!updated) {
				markOwnershipLost();
				return false;
			}
			return true;
		};
		const renewal = setInterval(() => {
			renewWrapupMarker({
				lastCompartmentEnd: getLastCompartmentEndMessage(deps.db, sessionId),
			});
		}, 60_000);
		try {
			sendCtxStatusMessage(pi, {
				title: "/ctx-wrapup",
				text: `## Magic Wrapup\n\nEligible history is about ${initialPlan.snapshot.trueRawEligibleTokens.toLocaleString()} tokens across approximately ${estimateChunks(initialPlan.snapshot.trueRawEligibleTokens, deps.historianChunkTokens)} historian chunk(s).`,
				level: "info",
			});

			const startEnd = getLastCompartmentEndMessage(deps.db, sessionId);
			const startCompartmentCount = getCompartments(deps.db, sessionId).length;
			let chunkIndex = 0;
			let lastEnd = startEnd;
			let failure: string | null = null;

			for (;;) {
				if (ownershipLost) {
					failure = `${ownershipLostReason}; wrapped up through message ${lastEnd}. Run /ctx-wrapup again to continue.`;
					break;
				}
				if (
					!renewWrapupMarker({
						chunkIndex,
						lastCompartmentEnd: getLastCompartmentEndMessage(
							deps.db,
							sessionId,
						),
					})
				) {
					failure = `${ownershipLostReason}; wrapped up through message ${lastEnd}. Run /ctx-wrapup again to continue.`;
					break;
				}

				const plan = resolveWrapupProtectedTailBoundary({
					db: deps.db,
					sessionId,
					mode: "manual-wrapup",
					contextLimit,
					executeThresholdPercentage,
					usage: { percentage: 0, inputTokens: 0 },
					usageSource: "manual-none",
					providerShapeVersion: "pi-folded-v1",
					cacheNamespace: `pi:${sessionId}`,
					messagesToKeep,
					anchorRawMessageCount: initialPlan.anchorRawMessageCount,
				});
				if (plan.snapshot.eligibleEndOrdinal <= plan.snapshot.offset) break;

				chunkIndex += 1;
				if (
					!renewWrapupMarker({
						chunkIndex,
						lastCompartmentEnd: getLastCompartmentEndMessage(
							deps.db,
							sessionId,
						),
						targetEligibleEndOrdinal: plan.targetEligibleEndOrdinal,
						expectedChunks: Math.max(
							chunkIndex,
							estimateChunks(
								plan.snapshot.trueRawEligibleTokens,
								deps.historianChunkTokens,
							),
						),
					})
				) {
					failure = `${ownershipLostReason}; wrapped up through message ${lastEnd}. Run /ctx-wrapup again to continue.`;
					break;
				}
				sendCtxStatusMessage(pi, {
					title: "/ctx-wrapup",
					text: `## Magic Wrapup\n\nChunk ${chunkIndex}: wrapping messages ${plan.snapshot.offset}-${plan.snapshot.eligibleEndOrdinal - 1} (~${plan.snapshot.trueRawEligibleTokens.toLocaleString()} eligible tokens remain).`,
					level: "info",
				});

				const leaseHolder = await acquireCompartmentLeaseEventually(
					deps.db,
					sessionId,
					renewWrapupMarker,
				);
				if (!leaseHolder) {
					failure = ownershipLost
						? `${ownershipLostReason}; wrapped up through message ${lastEnd}. Run /ctx-wrapup again to continue.`
						: "Another Magic Context rebuild started while wrapup was waiting. Run /ctx-wrapup again to continue.";
					break;
				}
				const leaseRenewal = setInterval(() => {
					renewCompartmentLease(deps.db, sessionId, leaseHolder);
				}, COMPARTMENT_LEASE_RENEWAL_MS);
				try {
					const runHistorian = deps.runPiHistorianForWrapup ?? runPiHistorian;
					await runHistorian({
						db: deps.db,
						sessionId,
						directory: ctx.cwd,
						provider,
						runner: deps.runner,
						historianModel: deps.historianModel as string,
						fallbackModels: deps.historianFallbacks,
						historianChunkTokens: deps.historianChunkTokens,
						boundarySnapshot: plan.snapshot,
						refreshBoundarySnapshot: () =>
							resolveWrapupProtectedTailBoundary({
								db: deps.db,
								sessionId,
								mode: "manual-wrapup",
								contextLimit,
								executeThresholdPercentage,
								usage: { percentage: 0, inputTokens: 0 },
								usageSource: "manual-none",
								providerShapeVersion: "pi-folded-v1",
								cacheNamespace: `pi:${sessionId}`,
								messagesToKeep,
								anchorRawMessageCount: initialPlan.anchorRawMessageCount,
							}).snapshot,
						currentContextLimit: contextLimit,
						historianTimeoutMs: deps.historianTimeoutMs,
						thinkingLevel: deps.historianThinkingLevel,
						memoryEnabled: deps.memoryEnabled,
						autoPromote: deps.autoPromote,
						userMemoriesEnabled: deps.userMemoriesEnabled,
						language: deps.language,
						compartmentLeaseHolderId: leaseHolder,
						readBranchEntries: () => readBranchEntries(ctx),
						notifyIssue: (text) =>
							sendCtxStatusMessage(pi, {
								title: "/ctx-wrapup",
								text,
								level: "warning",
							}),
						ensureProjectRegistered: ensureProjectRegisteredFromPiDirectory,
						forceDrainQuota: true,
						// The runner applies this only to the actual final chunk; token-capped
						// chunks are downgraded based on readSessionChunk().hasMore.
						forceKeepLastCompartment: true,
						onPublished: () => {
							updateStatusLine(ctx, { db: deps.db, projectIdentity: ctx.cwd });
							signalPiDeferredHistoryRefresh(sessionId);
							signalPiDeferredMaterialization(sessionId);
						},
					});
				} finally {
					clearInterval(leaseRenewal);
					releaseCompartmentLease(deps.db, sessionId, leaseHolder);
				}

				const afterEnd = getLastCompartmentEndMessage(deps.db, sessionId);
				if (afterEnd <= lastEnd) {
					failure = `No forward progress after chunk ${chunkIndex}; wrapped up through message ${lastEnd}. Run /ctx-wrapup again to continue.`;
					break;
				}
				lastEnd = afterEnd;
				const drainMarker =
					deps.drainPendingPiMarkerForWrapup ?? drainPendingPiMarker;
				const drained = drainMarker(deps.db, sessionId, ctx);
				if (!drained.ok) {
					failure = `Wrapped up through message ${lastEnd}, but Pi's pending compaction marker could not be applied yet (${drained.reason}). Run /ctx-wrapup again after the next context pass.`;
					break;
				}
			}

			const finalEnd = getLastCompartmentEndMessage(deps.db, sessionId);
			const finalCompartmentCount = getCompartments(deps.db, sessionId).length;
			const messagesWrapped = Math.max(0, finalEnd - startEnd);
			const compartmentsCreated = Math.max(
				0,
				finalCompartmentCount - startCompartmentCount,
			);
			if (failure) {
				return `## Magic Wrapup — Partial\n\nWrapped up ${messagesWrapped} messages into ${compartmentsCreated} compartments; ${failure}`;
			}
			try {
				clearEmergencyRecovery(deps.db, sessionId);
			} catch {
				// Best-effort: normal historian recovery disarm remains the backstop.
			}
			return `## Magic Wrapup\n\nWrapped up ${messagesWrapped} messages into ${compartmentsCreated} compartments. The compacted history is queued and materializes on your next message.`;
		} finally {
			clearInterval(renewal);
			releaseWrapupInProgress(deps.db, sessionId, holderId);
		}
	} finally {
		unregister();
	}
}

function resolvePiContextLimit(ctx: ExtensionCommandContext): number {
	const usage = ctx.getContextUsage?.();
	if (typeof usage?.contextWindow === "number" && usage.contextWindow > 0) {
		return usage.contextWindow;
	}
	if (
		typeof ctx.model?.contextWindow === "number" &&
		ctx.model.contextWindow > 0
	) {
		return ctx.model.contextWindow;
	}
	return 128_000;
}

async function acquireCompartmentLeaseEventually(
	db: ContextDatabase,
	sessionId: string,
	renewWrapupMarker: (
		updates: Parameters<typeof updateWrapupInProgress>[3],
	) => boolean,
): Promise<string | null> {
	for (;;) {
		const holderId = crypto.randomUUID();
		const lease = acquireCompartmentLease(db, sessionId, holderId);
		if (lease) return holderId;
		if (!renewWrapupMarker({})) return null;
		await new Promise((resolve) => setTimeout(resolve, LEASE_WAIT_MS));
	}
}

function estimateChunks(tokens: number, chunkTokens: number): number {
	return Math.max(1, Math.ceil(Math.max(0, tokens) / Math.max(1, chunkTokens)));
}

function formatExistingWrapup(
	state: ReturnType<typeof getWrapupInProgressState>,
): string {
	if (!state) {
		return "## Magic Wrapup — Skipped\n\nAnother /ctx-wrapup is already compacting this session. Wait for it to finish, then try again.";
	}
	return `## Magic Wrapup — Skipped\n\nAnother /ctx-wrapup is already compacting this session (chunk ${state.chunkIndex}/${Math.max(state.chunkIndex, state.expectedChunks)}, wrapped through message ${state.lastCompartmentEnd}). Wait for it to finish, then try again.`;
}

function readBranchEntries(ctx: ExtensionCommandContext): unknown[] {
	const getBranch = (ctx.sessionManager as { getBranch?: () => unknown })
		.getBranch;
	if (typeof getBranch !== "function") return [];
	const branch = getBranch.call(ctx.sessionManager) as
		| { entries?: unknown }
		| null
		| undefined;
	return Array.isArray(branch?.entries) ? branch.entries : [];
}

function appendCompaction(
	ctx: ExtensionCommandContext,
):
	| ((
			summary: string,
			firstKeptEntryId: string,
			tokensBefore: number,
			details?: unknown,
			fromHook?: boolean,
	  ) => string | undefined)
	| null {
	const candidate = (
		ctx.sessionManager as {
			appendCompaction?: (
				summary: string,
				firstKeptEntryId: string,
				tokensBefore: number,
				details?: unknown,
				fromHook?: boolean,
			) => string | undefined;
		}
	).appendCompaction;
	return typeof candidate === "function"
		? candidate.bind(ctx.sessionManager)
		: null;
}

function drainPendingPiMarker(
	db: ContextDatabase,
	sessionId: string,
	ctx: ExtensionCommandContext,
): { ok: true } | { ok: false; reason: string } {
	const pending = getPendingPiCompactionMarkerState(db, sessionId);
	if (!pending) return { ok: true };
	const append = appendCompaction(ctx);
	if (!append) return { ok: false, reason: "appendCompaction unavailable" };
	const outcome = applyDeferredPiCompactionMarker(
		{
			db,
			readBranchEntries: () => readBranchEntries(ctx),
			appendCompaction: append,
		},
		sessionId,
		pending,
	);
	if (outcome.kind === "retryable-failure") {
		return { ok: false, reason: outcome.error.message };
	}
	clearPendingPiCompactionMarkerStateIf(db, sessionId, pending);
	return { ok: true };
}
