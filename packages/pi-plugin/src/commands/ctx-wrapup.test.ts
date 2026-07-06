/// <reference types="bun-types" />

import { describe, expect, it, mock } from "bun:test";
import {
	appendCompartments,
	getCompartments,
	getLastCompartmentEndMessage,
} from "@magic-context/core/features/magic-context/compartment-storage";
import { runMigrations } from "@magic-context/core/features/magic-context/migrations";
import { initializeDatabase } from "@magic-context/core/features/magic-context/storage-db";
import {
	getPendingPiCompactionMarkerState,
	getWrapupInProgressState,
	setPendingPiCompactionMarkerState,
} from "@magic-context/core/features/magic-context/storage-meta-persisted";
import { Database } from "@magic-context/core/shared/sqlite";
import { closeQuietly } from "@magic-context/core/shared/sqlite-helpers";
import {
	consumeDeferredHistoryRefresh,
	consumeDeferredMaterialization,
} from "../context-handler";
import {
	parseWrapupArgs,
	type RegisterCtxWrapupDeps,
	runPiWrapup,
} from "./ctx-wrapup";

function createDb(): Database {
	const db = new Database(":memory:");
	initializeDatabase(db);
	runMigrations(db);
	return db;
}

function branch(count: number) {
	return Array.from({ length: count }, (_, index) => ({
		id: `m-${index + 1}`,
		type: "message",
		timestamp: index + 1,
		message: {
			role: "user",
			content: `message ${index + 1} alpha beta gamma delta`,
		},
	}));
}

function ctx(sessionId: string, count = 8) {
	const entries = branch(count);
	return {
		cwd: "/tmp/pi-wrapup",
		model: { provider: "anthropic", id: "claude" },
		sessionManager: {
			getSessionId: () => sessionId,
			getBranch: () => entries,
		},
		getContextUsage: () => ({ contextWindow: 20, tokens: 1, percent: 1 }),
		ui: { setStatus() {} },
	} as never;
}

function pi() {
	const sent: Array<{ message: { content: string } }> = [];
	return {
		api: {
			sendMessage(message: { content: string }) {
				sent.push({ message });
			},
		} as never,
		sent,
	};
}

function appendRange(
	db: Database,
	sessionId: string,
	start: number,
	end: number,
): void {
	if (end < start) return;
	appendCompartments(db, sessionId, [
		{
			sequence: getCompartments(db, sessionId).length,
			startMessage: start,
			endMessage: end,
			startMessageId: `m-${start}`,
			endMessageId: `m-${end}`,
			title: `Pi ${start}-${end}`,
			content: `Pi ${start}-${end}`,
		},
	]);
}

function deps(
	db: Database,
	overrides: Partial<RegisterCtxWrapupDeps> = {},
): RegisterCtxWrapupDeps {
	return {
		db,
		runner: {} as never,
		historianModel: "test/model",
		historianChunkTokens: 10,
		memoryEnabled: false,
		autoPromote: false,
		runPiHistorianForWrapup: mock(async (args) => {
			const sessionId = args.sessionId;
			const start = getLastCompartmentEndMessage(db, sessionId) + 1;
			const end = Math.min(
				args.boundarySnapshot.eligibleEndOrdinal - 1,
				start + 2,
			);
			appendRange(db, sessionId, start, end);
			args.onPublished?.();
		}),
		...overrides,
	};
}

describe("Pi /ctx-wrapup", () => {
	it("parses optional positive messages_to_keep", () => {
		expect(parseWrapupArgs("")).toEqual({ ok: true, messagesToKeep: 20 });
		expect(parseWrapupArgs(" 7 ")).toEqual({ ok: true, messagesToKeep: 7 });
		expect(parseWrapupArgs("0").ok).toBe(false);
		expect(parseWrapupArgs("two").ok).toBe(false);
	});

	it("stops on no progress and releases the durable marker", async () => {
		const db = createDb();
		try {
			const sessionId = "pi-no-progress";
			const result = await runPiWrapup(
				pi().api,
				deps(db, {
					runPiHistorianForWrapup: mock(async () => {}),
				}),
				ctx(sessionId, 8),
				sessionId,
				2,
			);

			expect(result).toContain("## Magic Wrapup — Partial");
			expect(result).toContain("No forward progress");
			expect(result).toContain("Run /ctx-wrapup again to continue");
			const row = db
				.prepare(
					"SELECT wrapup_in_progress_state FROM session_meta WHERE session_id = ?",
				)
				.get(sessionId) as { wrapup_in_progress_state: string | null } | null;
			expect(row?.wrapup_in_progress_state ?? null).toBeNull();
		} finally {
			closeQuietly(db);
		}
	});

	it("signals deferred history and materialization after a wrapup publish", async () => {
		const db = createDb();
		try {
			const sessionId = "pi-wrapup-signals";
			const result = await runPiWrapup(
				pi().api,
				deps(db, {
					runPiHistorianForWrapup: mock(async (args) => {
						appendRange(db, sessionId, 1, 3);
						args.onPublished?.();
					}),
				}),
				ctx(sessionId, 8),
				sessionId,
				2,
			);

			expect(result).toContain("## Magic Wrapup");
			expect(consumeDeferredHistoryRefresh(sessionId)).toBe(true);
			expect(consumeDeferredMaterialization(sessionId)).toBe(true);
		} finally {
			closeQuietly(db);
		}
	});

	it("aborts when wrapup marker ownership is lost and leaves the foreign marker", async () => {
		const db = createDb();
		try {
			const sessionId = "pi-ownership-lost";
			let calls = 0;
			const result = await runPiWrapup(
				pi().api,
				deps(db, {
					runPiHistorianForWrapup: mock(async (args) => {
						calls += 1;
						appendRange(db, sessionId, 1, 3);
						args.onPublished?.();
						const state = getWrapupInProgressState(db, sessionId);
						expect(state).not.toBeNull();
						db.prepare(
							"UPDATE session_meta SET wrapup_in_progress_state = ? WHERE session_id = ?",
						).run(
							JSON.stringify({
								...state,
								holderId: "foreign-holder",
								updatedAt: Date.now(),
								expiresAt: Date.now() + 60_000,
							}),
							sessionId,
						);
					}),
				}),
				ctx(sessionId, 10),
				sessionId,
				2,
			);

			expect(result).toContain("## Magic Wrapup — Partial");
			expect(result).toContain(
				"another process took over this session's wrapup",
			);
			expect(calls).toBe(1);
			expect(getWrapupInProgressState(db, sessionId)?.holderId).toBe(
				"foreign-holder",
			);
		} finally {
			closeQuietly(db);
		}
	});

	it("stops when the pending Pi marker cannot be drained after a chunk", async () => {
		const db = createDb();
		try {
			const sessionId = "pi-marker-pending";
			const result = await runPiWrapup(
				pi().api,
				deps(db, {
					runPiHistorianForWrapup: mock(async (args) => {
						appendRange(db, sessionId, 1, 3);
						setPendingPiCompactionMarkerState(db, sessionId, {
							firstKeptEntryId: "m-4",
							endMessageId: "m-3",
							ordinal: 3,
							tokensBefore: 123,
							summary: "pending marker",
							publishedAt: Date.now(),
						});
						args.onPublished?.();
					}),
				}),
				ctx(sessionId, 8),
				sessionId,
				2,
			);

			expect(result).toContain("## Magic Wrapup — Partial");
			expect(result).toContain(
				"pending compaction marker could not be applied yet",
			);
			expect(result).toContain("appendCompaction unavailable");
			expect(getLastCompartmentEndMessage(db, sessionId)).toBe(3);
			expect(getPendingPiCompactionMarkerState(db, sessionId)).toEqual(
				expect.objectContaining({ ordinal: 3, endMessageId: "m-3" }),
			);
			expect(consumeDeferredHistoryRefresh(sessionId)).toBe(true);
			expect(consumeDeferredMaterialization(sessionId)).toBe(true);
		} finally {
			closeQuietly(db);
		}
	});
});
