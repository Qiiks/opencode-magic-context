import { beforeEach, describe, expect, it } from "bun:test";
import type {
	ExtensionCommandContext,
	ExtensionUIContext,
} from "@earendil-works/pi-coding-agent";
import {
	__resetTodoSnapshotsForTests,
	getTodoSnapshot,
	registerTodoOverlay,
	registerTodosCommand,
	setTodoSnapshot,
	TodoOverlay,
} from "./todo-view-pi";
import { createTodowriteTool } from "./todowrite";

const identityTheme = {
	fg: (_color: string, text: string) => text,
	bg: (_color: string, text: string) => text,
	bold: (text: string) => text,
	italic: (text: string) => text,
	underline: (text: string) => text,
	inverse: (text: string) => text,
	strikethrough: (text: string) => text,
} as never;

function makeUi() {
	const setWidgetCalls: unknown[][] = [];
	const notifyCalls: unknown[][] = [];
	const ui = {
		setWidget: (...args: unknown[]) => setWidgetCalls.push(args),
		notify: (...args: unknown[]) => notifyCalls.push(args),
	} as unknown as ExtensionUIContext;
	return { ui, setWidgetCalls, notifyCalls };
}

function commandCtx(
	sessionId = "ses-test",
	ui = makeUi().ui,
): ExtensionCommandContext {
	return {
		hasUI: true,
		ui,
		sessionManager: { getSessionId: () => sessionId },
	} as never;
}

function widgetFactory(call: unknown[]) {
	return call[1] as (
		tui: { requestRender: () => void },
		theme: typeof identityTheme,
	) => { render: (width: number) => string[]; invalidate: () => void };
}

beforeEach(() => {
	__resetTodoSnapshotsForTests();
});

describe("todowrite tool rendering", () => {
	it("renders glyph lines while preserving the exact JSON text result", async () => {
		const tool = createTodowriteTool();
		const todos = [
			{ id: "a", content: "Plan", status: "pending", priority: "high" },
			{ id: "b", content: "Build", status: "in_progress", priority: "medium" },
			{ id: "c", content: "Done", status: "completed", priority: "low" },
			{ id: "d", content: "Drop", status: "cancelled", priority: "low" },
		];

		const result = await tool.execute(
			"call-1",
			{ todos } as never,
			undefined,
			undefined,
			{} as never,
		);

		expect(result.content).toEqual([
			{ type: "text", text: JSON.stringify(todos, null, 2) },
		]);
		const renderResult = tool.renderResult;
		const renderCall = tool.renderCall;
		if (!renderResult || !renderCall)
			throw new Error("todowrite render hooks missing");
		const rendered = renderResult(
			result,
			{ expanded: false, isPartial: false },
			identityTheme,
			{} as never,
		)
			.render(120)
			.join("\n");

		expect(rendered).toContain("○ #a Plan");
		expect(rendered).toContain("◐ #b Build");
		expect(rendered).toContain("✓ #c Done");
		expect(rendered).toContain("✗ #d Drop");
		expect(
			renderCall({ todos } as never, identityTheme, {} as never).render(80)[0],
		).toContain("Todos — 3 active");
	});
});

describe("/todos command", () => {
	it("prints grouped todos from the shared in-memory snapshot", async () => {
		const commands = new Map<
			string,
			{ handler: (args: string, ctx: ExtensionCommandContext) => Promise<void> }
		>();
		registerTodosCommand({
			registerCommand: (name, command) => commands.set(name, command),
		} as never);
		setTodoSnapshot("ses-command", [
			{ id: "p", content: "Plan", status: "pending" },
			{ id: "w", content: "Work", status: "in_progress" },
			{ id: "d", content: "Done", status: "completed" },
		]);
		const { ui, notifyCalls } = makeUi();

		await commands.get("todos")?.handler("", commandCtx("ses-command", ui));

		expect(notifyCalls).toHaveLength(1);
		const [text, level] = notifyCalls[0] as [string, string];
		expect(level).toBe("info");
		expect(text).toContain("1/3 completed · 1 in progress · 1 pending");
		expect(text).toContain("── Pending ──");
		expect(text).toContain("○ #p Plan");
		expect(text).toContain("── In Progress ──");
		expect(text).toContain("◐ #w Work");
		expect(text).toContain("── Completed ──");
		expect(text).toContain("✓ #d Done");
	});

	it("notifies when the session has no todos", async () => {
		const commands = new Map<
			string,
			{ handler: (args: string, ctx: ExtensionCommandContext) => Promise<void> }
		>();
		registerTodosCommand({
			registerCommand: (name, command) => commands.set(name, command),
		} as never);
		const { ui, notifyCalls } = makeUi();

		await commands.get("todos")?.handler("", commandCtx("empty-session", ui));

		expect(notifyCalls).toEqual([["No todos yet.", "info"]]);
	});
});

describe("TodoOverlay lifecycle", () => {
	it("registers once and refreshes with requestRender", () => {
		setTodoSnapshot("ses-overlay", [
			{ id: "1", content: "Plan", status: "pending" },
		]);
		const overlay = new TodoOverlay();
		const { ui, setWidgetCalls } = makeUi();
		overlay.setUICtx("ses-overlay", ui);

		overlay.update("ses-overlay");
		expect(setWidgetCalls).toHaveLength(1);
		expect(setWidgetCalls[0][0]).toBe("magic-context-todos");
		expect(typeof setWidgetCalls[0][1]).toBe("function");
		expect(setWidgetCalls[0][2]).toEqual({ placement: "aboveEditor" });

		const tui = { requestRender: () => requestRenderCount++ };
		let requestRenderCount = 0;
		widgetFactory(setWidgetCalls[0])(tui, identityTheme);
		overlay.update("ses-overlay");

		expect(setWidgetCalls).toHaveLength(1);
		expect(requestRenderCount).toBe(1);
	});

	it("auto-hides on empty snapshots", () => {
		setTodoSnapshot("ses-overlay", [
			{ id: "1", content: "Plan", status: "pending" },
		]);
		const overlay = new TodoOverlay();
		const { ui, setWidgetCalls } = makeUi();
		overlay.setUICtx("ses-overlay", ui);
		overlay.update("ses-overlay");

		setTodoSnapshot("ses-overlay", []);
		overlay.update("ses-overlay");

		expect(setWidgetCalls).toHaveLength(2);
		expect(setWidgetCalls[1]).toEqual(["magic-context-todos", undefined]);
	});

	it("re-registers after widget invalidation", () => {
		setTodoSnapshot("ses-overlay", [
			{ id: "1", content: "Plan", status: "pending" },
		]);
		const overlay = new TodoOverlay();
		const { ui, setWidgetCalls } = makeUi();
		overlay.setUICtx("ses-overlay", ui);
		overlay.update("ses-overlay");

		const widget = widgetFactory(setWidgetCalls[0])(
			{ requestRender: () => undefined },
			identityTheme,
		);
		widget.invalidate();
		overlay.update("ses-overlay");

		expect(setWidgetCalls).toHaveLength(2);
		expect(typeof setWidgetCalls[1][1]).toBe("function");
	});

	it("hides completed tasks from previous turns after showing them once", () => {
		setTodoSnapshot("ses-overlay", [
			{ id: "1", content: "Done", status: "completed" },
		]);
		const overlay = new TodoOverlay();
		const { ui, setWidgetCalls } = makeUi();
		overlay.setUICtx("ses-overlay", ui);
		overlay.update("ses-overlay");
		const widget = widgetFactory(setWidgetCalls[0])(
			{ requestRender: () => undefined },
			identityTheme,
		);

		expect(widget.render(120).join("\n")).toContain("Done");
		overlay.hideCompletedTasksFromPreviousTurn();

		expect(setWidgetCalls[setWidgetCalls.length - 1]).toEqual([
			"magic-context-todos",
			undefined,
		]);
		expect(widget.render(120)).toEqual([]);
	});

	it("seeds from session_meta.last_todo_state and clears on session switch", async () => {
		const handlers = new Map<
			string,
			(event: unknown, ctx: unknown) => Promise<void> | void
		>();
		registerTodoOverlay(
			{
				on: (event, handler) => handlers.set(event, handler as never),
			} as never,
			{
				readLastTodoState: () =>
					JSON.stringify([
						{ content: "Seeded", status: "pending", priority: "medium" },
					]),
			},
		);
		const { ui, setWidgetCalls } = makeUi();
		const ctx = {
			hasUI: true,
			ui,
			sessionManager: { getSessionId: () => "ses-seeded" },
		};

		await handlers.get("session_start")?.({ type: "session_start" }, ctx);

		expect(getTodoSnapshot("ses-seeded").todos).toEqual([
			{ content: "Seeded", status: "pending", priority: "medium" },
		]);
		expect(setWidgetCalls).toHaveLength(1);

		await handlers.get("session_before_switch")?.(
			{ type: "session_before_switch" },
			ctx,
		);

		expect(getTodoSnapshot("ses-seeded").todos).toEqual([]);
		expect(setWidgetCalls[setWidgetCalls.length - 1]).toEqual([
			"magic-context-todos",
			undefined,
		]);
	});

	it("caps content rows with a +N more tail", () => {
		setTodoSnapshot(
			"ses-overlay",
			Array.from({ length: 14 }, (_, index) => ({
				id: String(index + 1),
				content: `Task ${index + 1}`,
				status: "pending",
			})),
		);
		const overlay = new TodoOverlay();
		const { ui, setWidgetCalls } = makeUi();
		overlay.setUICtx("ses-overlay", ui);
		overlay.update("ses-overlay");
		const widget = widgetFactory(setWidgetCalls[0])(
			{ requestRender: () => undefined },
			identityTheme,
		);

		const lines = widget.render(120);
		expect(lines).toHaveLength(13); // header + 11 tasks + tail
		expect(lines[lines.length - 1]).toContain("+3 more");
	});
});
