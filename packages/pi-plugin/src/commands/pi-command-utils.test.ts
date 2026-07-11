import { describe, expect, it } from "bun:test";

import {
	CTX_STATUS_CUSTOM_TYPE,
	type CtxStatusEntryData,
	type PiMessageSender,
	registerCtxStatusEntryRenderer,
	sendCtxStatusMessage,
} from "./pi-command-utils";

describe("ctx-status entries", () => {
	it("appends model-invisible entry data instead of sending a message", () => {
		const appended: Array<{ customType: string; data: unknown }> = [];
		let sent = 0;
		const pi = {
			registerEntryRenderer() {},
			appendEntry(customType: string, data?: unknown) {
				appended.push({ customType, data });
			},
			sendMessage() {
				sent += 1;
			},
		} as unknown as PiMessageSender;

		registerCtxStatusEntryRenderer(pi);
		sendCtxStatusMessage(
			pi,
			{ title: "Magic Embed", text: "Embedding history…", level: "info" },
			{ completed: 2 },
		);

		expect(appended).toEqual([
			{
				customType: CTX_STATUS_CUSTOM_TYPE,
				data: {
					title: "Magic Embed",
					text: "Embedding history…",
					level: "info",
					details: { completed: 2 },
				},
			},
		]);
		expect(sent).toBe(0);
	});

	it("registers one ctx-status entry renderer and ignores malformed data", () => {
		let customType = "";
		let renderer:
			| ((
					entry: { data?: CtxStatusEntryData },
					options: unknown,
					theme: unknown,
			  ) => unknown)
			| undefined;
		const pi = {
			registerEntryRenderer(type: string, value: typeof renderer) {
				customType = type;
				renderer = value;
			},
			appendEntry() {},
		} as unknown as PiMessageSender;

		expect(registerCtxStatusEntryRenderer(pi)).toBe(true);
		expect(customType).toBe(CTX_STATUS_CUSTOM_TYPE);
		expect(renderer).toBeDefined();
		expect(
			renderer?.({ data: undefined }, { expanded: false }, {}),
		).toBeUndefined();
	});

	it("keeps statuses visible on the Pi 0.80.2 API without entry renderers", () => {
		let appended = 0;
		let sentMessage: { customType?: string; content?: string } | undefined;
		const pi = {
			appendEntry() {
				appended += 1;
			},
			sendMessage(message: { customType?: string; content?: string }) {
				sentMessage = message;
			},
		} as unknown as PiMessageSender;

		expect(registerCtxStatusEntryRenderer(pi)).toBe(false);
		sendCtxStatusMessage(pi, { title: "Magic Status", text: "Ready" });

		expect(appended).toBe(0);
		expect(sentMessage).toMatchObject({
			customType: CTX_STATUS_CUSTOM_TYPE,
			content: "Ready",
		});
	});
});
