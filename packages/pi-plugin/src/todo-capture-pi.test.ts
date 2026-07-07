import { beforeEach, describe, expect, it } from "bun:test";
import {
	getOrCreateSessionMeta,
	updateSessionMeta,
} from "@magic-context/core/features/magic-context/storage";
import { closeQuietly } from "@magic-context/core/shared/sqlite-helpers";

import {
	capturePiTodowriteArgsIfCompatible,
	capturePiTodowriteMessageIfCompatible,
} from "./index";
import { assistantToolCall, createTestDb } from "./test-utils.test";
import {
	__resetTodoSnapshotsForTests,
	getTodoSnapshot,
} from "./tools/todo-view-pi";

beforeEach(() => {
	__resetTodoSnapshotsForTests();
});

describe("Pi todowrite capture compatibility", () => {
	it("tool_execution_start capture ignores foreign status values", () => {
		const db = createTestDb();
		const previousState =
			'[{"content":"Keep","status":"pending","priority":"medium"}]';
		let overlayUpdates = 0;
		try {
			updateSessionMeta(db, "ses-args", { lastTodoState: previousState });

			const captured = capturePiTodowriteArgsIfCompatible({
				db,
				sessionId: "ses-args",
				todos: [{ content: "Foreign", status: "done" }],
				todowriteEnabled: true,
				todoOverlay: { update: () => overlayUpdates++ },
				persist: true,
			});

			expect(captured).toBe(false);
			expect(getOrCreateSessionMeta(db, "ses-args").lastTodoState).toBe(
				previousState,
			);
			expect(getTodoSnapshot("ses-args").todos).toEqual([]);
			expect(overlayUpdates).toBe(0);
		} finally {
			closeQuietly(db);
		}
	});

	it("message_end capture ignores foreign status values", () => {
		const db = createTestDb();
		const previousState =
			'[{"content":"Keep","status":"pending","priority":"medium"}]';
		let overlayUpdates = 0;
		try {
			updateSessionMeta(db, "ses-message-status", {
				lastTodoState: previousState,
			});

			const captured = capturePiTodowriteMessageIfCompatible({
				db,
				sessionId: "ses-message-status",
				message: assistantToolCall("call-1", "todowrite", {
					todos: [{ content: "Foreign", status: "done" }],
				}),
				todowriteEnabled: true,
				todoOverlay: { update: () => overlayUpdates++ },
				persist: true,
			});

			expect(captured).toBe(false);
			expect(
				getOrCreateSessionMeta(db, "ses-message-status").lastTodoState,
			).toBe(previousState);
			expect(getTodoSnapshot("ses-message-status").todos).toEqual([]);
			expect(overlayUpdates).toBe(0);
		} finally {
			closeQuietly(db);
		}
	});

	it("message_end capture ignores absent or non-array todos", () => {
		const db = createTestDb();
		const previousState =
			'[{"content":"Keep","status":"pending","priority":"medium"}]';
		let overlayUpdates = 0;
		try {
			updateSessionMeta(db, "ses-message-shape", {
				lastTodoState: previousState,
			});

			expect(
				capturePiTodowriteMessageIfCompatible({
					db,
					sessionId: "ses-message-shape",
					message: assistantToolCall("call-1", "todowrite", {}),
					todowriteEnabled: true,
					todoOverlay: { update: () => overlayUpdates++ },
					persist: true,
				}),
			).toBe(false);
			expect(
				capturePiTodowriteMessageIfCompatible({
					db,
					sessionId: "ses-message-shape",
					message: assistantToolCall("call-2", "todowrite", {
						todos: { content: "Not an array", status: "pending" },
					}),
					todowriteEnabled: true,
					todoOverlay: { update: () => overlayUpdates++ },
					persist: true,
				}),
			).toBe(false);

			expect(
				getOrCreateSessionMeta(db, "ses-message-shape").lastTodoState,
			).toBe(previousState);
			expect(getTodoSnapshot("ses-message-shape").todos).toEqual([]);
			expect(overlayUpdates).toBe(0);
		} finally {
			closeQuietly(db);
		}
	});

	it("message_end capture preserves interop for compatible foreign todowrite payloads", () => {
		const db = createTestDb();
		let overlayUpdates = 0;
		const todos = [{ content: "Interop", status: "pending", priority: "high" }];
		try {
			const captured = capturePiTodowriteMessageIfCompatible({
				db,
				sessionId: "ses-message-valid",
				message: assistantToolCall("call-1", "todowrite", { todos }),
				todowriteEnabled: true,
				todoOverlay: { update: () => overlayUpdates++ },
				persist: true,
			});

			expect(captured).toBe(true);
			expect(
				getOrCreateSessionMeta(db, "ses-message-valid").lastTodoState,
			).toBe('[{"content":"Interop","status":"pending","priority":"high"}]');
			expect(getTodoSnapshot("ses-message-valid").todos).toEqual(todos);
			expect(overlayUpdates).toBe(1);
		} finally {
			closeQuietly(db);
		}
	});
});
