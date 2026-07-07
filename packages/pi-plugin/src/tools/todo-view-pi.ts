import type {
	ExtensionAPI,
	ExtensionCommandContext,
	ExtensionUIContext,
	Theme,
} from "@earendil-works/pi-coding-agent";
import {
	type Component,
	type TUI,
	truncateToWidth,
} from "@earendil-works/pi-tui";

export const TODO_TOOL_NAME = "todowrite";
export const TODOS_COMMAND_NAME = "todos";

const WIDGET_KEY = "magic-context-todos";
const MAX_OVERLAY_CONTENT_ROWS = 12;

export const TODO_STATUSES = [
	"pending",
	"in_progress",
	"completed",
	"cancelled",
] as const;
const TODO_PRIORITIES = ["high", "medium", "low"] as const;

export type TodoStatus = (typeof TODO_STATUSES)[number];
export type TodoPriority = (typeof TODO_PRIORITIES)[number];

export interface TodoItem {
	content: string;
	status: TodoStatus;
	priority?: TodoPriority;
	id?: string;
}

export interface TodoSnapshot {
	todos: TodoItem[];
}

interface TodoCounts {
	total: number;
	pending: number;
	inProgress: number;
	completed: number;
	cancelled: number;
}

const STATUS_GLYPH: Record<TodoStatus, string> = {
	pending: "○",
	in_progress: "◐",
	completed: "✓",
	cancelled: "✗",
};

const STATUS_COLOR: Record<TodoStatus, Parameters<Theme["fg"]>[0]> = {
	pending: "dim",
	in_progress: "warning",
	completed: "success",
	cancelled: "error",
};

const snapshotsBySession = new Map<string, TodoSnapshot>();

function isTodoStatus(value: unknown): value is TodoStatus {
	return TODO_STATUSES.includes(value as TodoStatus);
}

function isTodoPriority(value: unknown): value is TodoPriority {
	return TODO_PRIORITIES.includes(value as TodoPriority);
}

function cloneTodos(todos: readonly TodoItem[]): TodoItem[] {
	return todos.map((todo) => ({ ...todo }));
}

export function parseTodos(input: unknown): TodoItem[] | null {
	if (!Array.isArray(input)) return null;
	const todos: TodoItem[] = [];
	for (const item of input) {
		if (item === null || typeof item !== "object") return null;
		const raw = item as Record<string, unknown>;
		if (typeof raw.content !== "string" || !isTodoStatus(raw.status)) {
			return null;
		}
		const todo: TodoItem = {
			content: raw.content,
			status: raw.status,
		};
		if (isTodoPriority(raw.priority)) todo.priority = raw.priority;
		if (typeof raw.id === "string" && raw.id.length > 0) todo.id = raw.id;
		todos.push(todo);
	}
	return todos;
}

export function parseTodoStateJson(
	stateJson: string | null | undefined,
): TodoItem[] | null {
	if (!stateJson) return null;
	try {
		return parseTodos(JSON.parse(stateJson));
	} catch {
		return null;
	}
}

export function setTodoSnapshot(sessionId: string, todos: unknown): boolean {
	const parsed = parseTodos(todos);
	if (parsed === null) return false;
	snapshotsBySession.set(sessionId, { todos: parsed });
	return true;
}

export function seedTodoSnapshotFromStateJson(
	sessionId: string,
	stateJson: string | null | undefined,
): boolean {
	const parsed = parseTodoStateJson(stateJson);
	if (parsed === null) {
		snapshotsBySession.delete(sessionId);
		return false;
	}
	snapshotsBySession.set(sessionId, { todos: parsed });
	return true;
}

export function getTodoSnapshot(sessionId: string | undefined): TodoSnapshot {
	if (!sessionId) return { todos: [] };
	const snapshot = snapshotsBySession.get(sessionId);
	return { todos: snapshot ? cloneTodos(snapshot.todos) : [] };
}

export function clearTodoSnapshot(sessionId: string): void {
	snapshotsBySession.delete(sessionId);
}

export function __resetTodoSnapshotsForTests(): void {
	snapshotsBySession.clear();
}

function getSessionId(ctx: {
	sessionManager?: { getSessionId?: () => string | undefined };
}): string | undefined {
	try {
		const id = ctx.sessionManager?.getSessionId?.();
		return typeof id === "string" && id.length > 0 ? id : undefined;
	} catch {
		return undefined;
	}
}

function countTodos(todos: readonly TodoItem[]): TodoCounts {
	const counts: TodoCounts = {
		total: todos.length,
		pending: 0,
		inProgress: 0,
		completed: 0,
		cancelled: 0,
	};
	for (const todo of todos) {
		switch (todo.status) {
			case "pending":
				counts.pending++;
				break;
			case "in_progress":
				counts.inProgress++;
				break;
			case "completed":
				counts.completed++;
				break;
			case "cancelled":
				counts.cancelled++;
				break;
		}
	}
	return counts;
}

function activeTitleCount(todos: readonly TodoItem[]): number {
	// OpenCode's todowrite title excludes only completed todos; cancelled items
	// remain in the model-visible active count for wire-shape parity.
	return todos.filter((todo) => todo.status !== "completed").length;
}

function formatCounts(counts: TodoCounts): string {
	if (counts.total === 0) return "No todos";
	const parts = [`${counts.completed}/${counts.total} completed`];
	if (counts.inProgress > 0) parts.push(`${counts.inProgress} in progress`);
	if (counts.pending > 0) parts.push(`${counts.pending} pending`);
	if (counts.cancelled > 0) parts.push(`${counts.cancelled} cancelled`);
	return parts.join(" · ");
}

function formatTodoLine(
	todo: TodoItem,
	theme: Theme,
	options: { showId?: boolean } = {},
): string {
	const glyph = theme.fg(STATUS_COLOR[todo.status], STATUS_GLYPH[todo.status]);
	const id =
		options.showId && todo.id ? `${theme.fg("accent", `#${todo.id}`)} ` : "";
	const color =
		todo.status === "pending" || todo.status === "in_progress" ? "text" : "dim";
	let content = theme.fg(color, todo.content);
	if (todo.status === "completed" || todo.status === "cancelled") {
		content = theme.strikethrough(content);
	}
	return `${glyph} ${id}${content}`;
}

function formatCommandLine(todo: TodoItem): string {
	const id = todo.id ? `#${todo.id} ` : "";
	return `  ${STATUS_GLYPH[todo.status]} ${id}${todo.content}`;
}

function lineComponent(renderLines: (width: number) => string[]): Component {
	return {
		render: renderLines,
		invalidate() {},
	};
}

export function renderTodowriteCall(
	args: { todos?: unknown },
	theme: Theme,
): Component {
	const todos = parseTodos(args.todos) ?? [];
	const active = activeTitleCount(todos);
	const activeColor: Parameters<Theme["fg"]>[0] =
		active > 0 ? "warning" : "success";
	const line = `${theme.fg("toolTitle", theme.bold("Todos"))} ${theme.fg("muted", "—")} ${theme.fg(activeColor, `${active} active`)}`;
	return lineComponent((width) => [truncateToWidth(line, width, "…")]);
}

export function renderTodowriteResult(
	result: { details?: unknown; content?: unknown },
	theme: Theme,
): Component {
	const detailsTodos = (result.details as { todos?: unknown } | undefined)
		?.todos;
	let todos = parseTodos(detailsTodos);
	if (todos === null && Array.isArray(result.content)) {
		const text = result.content.find(
			(part): part is { type: "text"; text: string } =>
				part !== null &&
				typeof part === "object" &&
				(part as { type?: unknown }).type === "text" &&
				typeof (part as { text?: unknown }).text === "string",
		)?.text;
		if (text) {
			try {
				todos = parseTodos(JSON.parse(text));
			} catch {
				// Leave the renderer empty if the tool result is not a todo list.
			}
		}
	}
	const renderedTodos = todos ?? [];
	return lineComponent((width) => {
		if (renderedTodos.length === 0) return [theme.fg("dim", "No todos")];
		return renderedTodos.map((todo) =>
			truncateToWidth(
				formatTodoLine(todo, theme, { showId: true }),
				width,
				"…",
			),
		);
	});
}

export function registerTodosCommand(
	pi: Pick<ExtensionAPI, "registerCommand">,
): void {
	pi.registerCommand(TODOS_COMMAND_NAME, {
		description: "Show the current Magic Context todo list",
		handler: async (_args: string, ctx: ExtensionCommandContext) => {
			if (!ctx.hasUI) {
				ctx.ui.notify("The /todos command requires interactive UI.", "error");
				return;
			}
			const sessionId = getSessionId(ctx);
			const todos = getTodoSnapshot(sessionId).todos;
			if (todos.length === 0) {
				ctx.ui.notify("No todos yet.", "info");
				return;
			}

			const counts = countTodos(todos);
			const lines = [formatCounts(counts)];
			const groups: Array<[TodoStatus, string]> = [
				["pending", "── Pending ──"],
				["in_progress", "── In Progress ──"],
				["completed", "── Completed ──"],
				["cancelled", "── Cancelled ──"],
			];
			for (const [status, heading] of groups) {
				const group = todos.filter((todo) => todo.status === status);
				if (group.length === 0) continue;
				lines.push(heading);
				for (const todo of group) lines.push(formatCommandLine(todo));
			}

			ctx.ui.notify(lines.join("\n"), "info");
		},
	});
}

function todoKey(todo: TodoItem, index: number): string {
	return todo.id ? `id:${todo.id}` : `pos:${index}:${todo.content}`;
}

function isOverlayLive(todo: TodoItem): boolean {
	return todo.status === "pending" || todo.status === "in_progress";
}

export class TodoOverlay {
	private uiCtx: ExtensionUIContext | undefined;
	private sessionId: string | undefined;
	private widgetRegistered = false;
	private tui: TUI | undefined;
	private completedTaskIdsPendingHide = new Set<string>();
	private hiddenCompletedTaskIds = new Set<string>();
	private lastTodoKeys = new Set<string>();

	setUICtx(sessionId: string, ctx: ExtensionUIContext): void {
		// Pi invalidates widget instances on /reload and session replacement. The
		// factory-form widget below can recover only if we forget the old registration
		// when either the UI proxy or the foreground session identity changes.
		if (ctx !== this.uiCtx || sessionId !== this.sessionId) {
			this.uiCtx = ctx;
			this.sessionId = sessionId;
			this.widgetRegistered = false;
			this.tui = undefined;
			this.resetCompletedDisplayState();
		}
	}

	update(sessionId = this.sessionId): void {
		if (!this.uiCtx || !this.sessionId || sessionId !== this.sessionId) return;
		const todos = getTodoSnapshot(this.sessionId).todos;
		this.pruneCompletedDisplayState(todos);
		const visible = this.selectOverlayTodos(todos);

		if (visible.length === 0) {
			if (this.widgetRegistered) {
				this.uiCtx.setWidget(WIDGET_KEY, undefined);
				this.widgetRegistered = false;
				this.tui = undefined;
			}
			return;
		}

		if (!this.widgetRegistered) {
			// Use factory-form registration once and read module state at render time.
			// Re-registering on every todowrite fights Pi's widget lifecycle; refreshing
			// the existing factory via requestRender keeps /reload invalidation and TUI
			// resize handling reliable.
			this.uiCtx.setWidget(
				WIDGET_KEY,
				(tui, theme) => {
					this.tui = tui;
					return {
						render: (width: number) => this.renderWidget(theme, width),
						invalidate: () => {
							this.widgetRegistered = false;
							this.tui = undefined;
						},
					};
				},
				{ placement: "aboveEditor" },
			);
			this.widgetRegistered = true;
		} else {
			this.tui?.requestRender();
		}
	}

	resetCompletedDisplayState(): void {
		this.completedTaskIdsPendingHide.clear();
		this.hiddenCompletedTaskIds.clear();
		this.lastTodoKeys.clear();
	}

	hideCompletedTasksFromPreviousTurn(): void {
		if (this.completedTaskIdsPendingHide.size === 0) return;
		for (const taskId of this.completedTaskIdsPendingHide) {
			this.hiddenCompletedTaskIds.add(taskId);
		}
		this.completedTaskIdsPendingHide.clear();
		this.update();
	}

	disposeSession(sessionId: string): void {
		if (this.sessionId !== sessionId) return;
		this.dispose();
	}

	dispose(): void {
		if (this.uiCtx) this.uiCtx.setWidget(WIDGET_KEY, undefined);
		this.widgetRegistered = false;
		this.tui = undefined;
		this.uiCtx = undefined;
		this.sessionId = undefined;
		this.resetCompletedDisplayState();
	}

	private pruneCompletedDisplayState(todos: readonly TodoItem[]): void {
		const currentKeys = new Set(
			todos.map((todo, index) => todoKey(todo, index)),
		);
		const hasSharedKeys = [...currentKeys].some((key) =>
			this.lastTodoKeys.has(key),
		);
		if (this.lastTodoKeys.size > 0 && currentKeys.size > 0 && !hasSharedKeys) {
			this.resetCompletedDisplayState();
		}
		this.lastTodoKeys = currentKeys;
		const completedKeys = new Set(
			todos
				.map((todo, index) => ({ todo, key: todoKey(todo, index) }))
				.filter(({ todo }) => todo.status === "completed")
				.map(({ key }) => key),
		);
		for (const taskId of this.completedTaskIdsPendingHide) {
			if (!completedKeys.has(taskId))
				this.completedTaskIdsPendingHide.delete(taskId);
		}
		for (const taskId of this.hiddenCompletedTaskIds) {
			if (!completedKeys.has(taskId))
				this.hiddenCompletedTaskIds.delete(taskId);
		}
	}

	private selectOverlayTodos(
		todos: readonly TodoItem[],
	): Array<{ todo: TodoItem; key: string }> {
		return todos
			.map((todo, index) => ({ todo, key: todoKey(todo, index) }))
			.filter(({ todo, key }) => {
				if (isOverlayLive(todo)) return true;
				return (
					todo.status === "completed" && !this.hiddenCompletedTaskIds.has(key)
				);
			});
	}

	private renderWidget(theme: Theme, width: number): string[] {
		if (!this.sessionId) return [];
		const todos = getTodoSnapshot(this.sessionId).todos;
		this.pruneCompletedDisplayState(todos);
		const overlayTodos = this.selectOverlayTodos(todos);
		if (overlayTodos.length === 0) return [];

		const counts = countTodos(todos);
		const hasInProgress = counts.inProgress > 0;
		const headingColor: Parameters<Theme["fg"]>[0] = hasInProgress
			? "accent"
			: "dim";
		const headingIcon = hasInProgress ? "●" : "○";
		const heading = `${theme.fg(headingColor, headingIcon)} ${theme.fg(headingColor, "Todos")} ${theme.fg("muted", "—")} ${theme.fg("dim", formatCounts(counts))}`;
		const truncate = (line: string) => truncateToWidth(line, width, "…");
		const lines = [truncate(heading)];

		const hasTruncatedTail = overlayTodos.length > MAX_OVERLAY_CONTENT_ROWS;
		const visibleRows = hasTruncatedTail
			? MAX_OVERLAY_CONTENT_ROWS - 1
			: MAX_OVERLAY_CONTENT_ROWS;
		const truncatedTail = Math.max(0, overlayTodos.length - visibleRows);
		const visible = overlayTodos.slice(0, visibleRows);

		for (const [index, { todo, key }] of visible.entries()) {
			const isLast = index === visible.length - 1 && truncatedTail === 0;
			const branch = theme.fg("dim", isLast ? "└─" : "├─");
			lines.push(
				truncate(`${branch} ${formatTodoLine(todo, theme, { showId: true })}`),
			);
			if (todo.status === "completed")
				this.completedTaskIdsPendingHide.add(key);
		}

		if (truncatedTail > 0) {
			lines.push(
				truncate(
					`${theme.fg("dim", "└─")} ${theme.fg("dim", `+${truncatedTail} more`)}`,
				),
			);
		}

		return lines;
	}
}

export function registerTodoOverlay(
	pi: Pick<ExtensionAPI, "on">,
	deps: { readLastTodoState: (sessionId: string) => string | null | undefined },
): TodoOverlay {
	const overlay = new TodoOverlay();

	pi.on("session_start", async (_event, ctx) => {
		const sessionId = getSessionId(ctx);
		if (!sessionId) return;
		seedTodoSnapshotFromStateJson(sessionId, deps.readLastTodoState(sessionId));
		if (!ctx.hasUI) return;
		overlay.setUICtx(sessionId, ctx.ui);
		overlay.update(sessionId);
	});

	pi.on("agent_start", async () => {
		overlay.hideCompletedTasksFromPreviousTurn();
	});

	pi.on("session_shutdown", async (_event, ctx) => {
		const sessionId = getSessionId(ctx);
		if (!sessionId) return;
		clearTodoSnapshot(sessionId);
		overlay.disposeSession(sessionId);
	});

	pi.on("session_before_switch", async (_event, ctx) => {
		const sessionId = getSessionId(ctx);
		if (!sessionId) return;
		clearTodoSnapshot(sessionId);
		overlay.disposeSession(sessionId);
	});

	return overlay;
}

export function registerTodoStateLifecycle(
	pi: Pick<ExtensionAPI, "on">,
	deps: { readLastTodoState: (sessionId: string) => string | null | undefined },
): void {
	pi.on("session_start", async (_event, ctx) => {
		const sessionId = getSessionId(ctx);
		if (!sessionId) return;
		seedTodoSnapshotFromStateJson(sessionId, deps.readLastTodoState(sessionId));
	});

	pi.on("session_shutdown", async (_event, ctx) => {
		const sessionId = getSessionId(ctx);
		if (sessionId) clearTodoSnapshot(sessionId);
	});

	pi.on("session_before_switch", async (_event, ctx) => {
		const sessionId = getSessionId(ctx);
		if (sessionId) clearTodoSnapshot(sessionId);
	});
}
