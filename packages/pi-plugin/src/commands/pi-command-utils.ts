import type {
	CustomEntry,
	ExtensionAPI,
	ExtensionCommandContext,
	Theme,
} from "@earendil-works/pi-coding-agent";
import { Box, type Component, Text } from "@earendil-works/pi-tui";

export const CTX_STATUS_CUSTOM_TYPE = "ctx-status";

export type CtxStatusLevel = "info" | "success" | "warning" | "error";

export interface CtxStatusEntryData {
	title: string;
	text: string;
	level?: CtxStatusLevel;
	details?: unknown;
}

export type CtxStatusMessageContent = CtxStatusEntryData;

type CtxStatusEntryRenderer = (
	entry: CustomEntry<CtxStatusEntryData>,
	options: { expanded: boolean },
	theme: Theme,
) => Component | undefined;

type PiEntryRendererRegistration = {
	registerEntryRenderer?: <T = unknown>(
		customType: string,
		renderer: (
			entry: CustomEntry<T>,
			options: { expanded: boolean },
			theme: Theme,
		) => Component | undefined,
	) => void;
};

export type PiMessageSender = Pick<ExtensionAPI, "appendEntry"> &
	Partial<Pick<ExtensionAPI, "sendMessage">> &
	PiEntryRendererRegistration;

const failedRendererRegistrations = new WeakSet<object>();

export function resolveSessionId(
	ctx: ExtensionCommandContext,
): string | undefined {
	const sm = ctx.sessionManager;
	const getSessionId = (sm as { getSessionId?: () => string | undefined })
		.getSessionId;
	if (typeof getSessionId !== "function") return undefined;
	try {
		const id = getSessionId.call(sm);
		return typeof id === "string" && id.length > 0 ? id : undefined;
	} catch {
		return undefined;
	}
}

function statusTitleColor(level: CtxStatusLevel | undefined) {
	switch (level) {
		case "success":
			return "success" as const;
		case "warning":
			return "warning" as const;
		case "error":
			return "error" as const;
		default:
			return "accent" as const;
	}
}

export const renderCtxStatusEntry: CtxStatusEntryRenderer = (
	entry,
	_options,
	theme,
) => {
	const data = entry?.data;
	if (
		!data ||
		typeof data !== "object" ||
		typeof data.title !== "string" ||
		typeof data.text !== "string"
	) {
		return undefined;
	}

	const title = theme.bold(
		theme.fg(statusTitleColor(data.level), `[${data.title}]`),
	);
	const body = theme.fg("customMessageText", data.text);
	const box = new Box(1, 0, (text) => theme.bg("customMessageBg", text));
	box.addChild(new Text(`${title}\n${body}`));
	return box;
};

/**
 * Register the model-invisible status-entry renderer when the Pi runtime supports it.
 * Pi 0.80.2 exposes appendEntry but not registerEntryRenderer, so callers retain the
 * legacy visible-message fallback until both halves of the API are available.
 */
export function registerCtxStatusEntryRenderer(pi: PiMessageSender): boolean {
	if (typeof pi.registerEntryRenderer !== "function") return false;
	try {
		pi.registerEntryRenderer<CtxStatusEntryData>(
			CTX_STATUS_CUSTOM_TYPE,
			renderCtxStatusEntry,
		);
		failedRendererRegistrations.delete(pi);
		return true;
	} catch {
		failedRendererRegistrations.add(pi);
		return false;
	}
}

export function sendCtxStatusMessage(
	pi: PiMessageSender,
	content: CtxStatusMessageContent,
	details?: unknown,
): void {
	const data: CtxStatusEntryData = {
		...content,
		details: details ?? content.details,
	};

	if (
		typeof pi.registerEntryRenderer === "function" &&
		!failedRendererRegistrations.has(pi)
	) {
		// Plain custom entries are persisted and rendered without entering Pi's
		// model context, so they cannot steer an in-flight agent turn.
		pi.appendEntry<CtxStatusEntryData>(CTX_STATUS_CUSTOM_TYPE, data);
		return;
	}

	if (typeof pi.sendMessage === "function") {
		// Compatibility for Pi versions that cannot render custom entries. This is
		// model-visible and may steer a stream, but keeps user status output visible.
		pi.sendMessage(
			{
				customType: CTX_STATUS_CUSTOM_TYPE,
				content: content.text,
				display: true,
				details: data,
			},
			{ triggerTurn: false },
		);
		return;
	}

	// Append-only test doubles and future non-interactive APIs can still persist the
	// status safely even when they do not expose renderer registration or sendMessage.
	pi.appendEntry<CtxStatusEntryData>(CTX_STATUS_CUSTOM_TYPE, data);
}
