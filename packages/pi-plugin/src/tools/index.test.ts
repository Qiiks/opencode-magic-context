import { describe, expect, it } from "bun:test";
import { closeQuietly } from "@magic-context/core/shared/sqlite-helpers";
import { createTestDb } from "../test-utils.test";
import { registerMagicContextTools } from "./index";

describe("registerMagicContextTools", () => {
	it("can omit ctx_memory for retrieval-only sidekick subagents", () => {
		const db = createTestDb();
		try {
			const registered: string[] = [];
			const pi = {
				registerTool: (tool: { name: string }) => {
					registered.push(tool.name);
				},
			} as never;

			registerMagicContextTools(pi, {
				db,
				memoryToolEnabled: false,
				sessionScopedToolsDisabled: true,
			});

			expect(registered).toContain("ctx_search");
			expect(registered).not.toContain("ctx_memory");
			expect(registered).not.toContain("ctx_note");
			expect(registered).not.toContain("ctx_expand");
		} finally {
			closeQuietly(db);
		}
	});

	it("registered tools resolve smart-note gating from the invocation cwd", async () => {
		const db = createTestDb();
		try {
			const registered = new Map<
				string,
				{ execute: (...args: never[]) => unknown }
			>();
			const pi = {
				registerTool: (tool: {
					name: string;
					execute: (...args: never[]) => unknown;
				}) => {
					registered.set(tool.name, tool);
				},
			} as never;

			registerMagicContextTools(pi, {
				db,
				dreamerEnabled: false,
				resolveDreamerEnabled: (ctx) => ctx.cwd === "/tmp/project-b",
			});

			const noteTool = registered.get("ctx_note");
			expect(noteTool).toBeDefined();
			const result = await noteTool?.execute(
				"call-1" as never,
				{
					action: "write",
					content: "Project B smart note",
					surface_condition: "When project B condition is true",
				} as never,
				new AbortController().signal as never,
				undefined as never,
				{
					cwd: "/tmp/project-b",
					sessionManager: { getSessionId: () => "ses-tool-cd" },
				} as never,
			);

			expect(
				(result as { isError?: boolean } | undefined)?.isError,
			).toBeUndefined();
		} finally {
			closeQuietly(db);
		}
	});
});
