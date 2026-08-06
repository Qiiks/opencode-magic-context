import { describe, expect, test } from "bun:test";
import { resolvePiUsableContextLimit } from "./pi-context-limit";

describe("resolvePiUsableContextLimit", () => {
	test("reserves Pi model maxTokens on shared-window providers", () => {
		expect(
			resolvePiUsableContextLimit({
				rawContextWindow: 122_880,
				model: {
					provider: "openai",
					id: "reporter-model",
					contextWindow: 122_880,
					maxTokens: 16_384,
				},
			}),
		).toBe(106_496);
	});

	test("keeps Google Antigravity's separate output quota unchanged", () => {
		expect(
			resolvePiUsableContextLimit({
				rawContextWindow: 1_048_576,
				model: {
					provider: "google-antigravity",
					id: "gemini-2.5-pro",
					maxTokens: 65_536,
				},
			}),
		).toBe(1_048_576);
	});

	test("applies detected wire truth before output reservation", () => {
		expect(
			resolvePiUsableContextLimit({
				rawContextWindow: 200_000,
				detectedContextLimit: 120_000,
				model: { provider: "anthropic", id: "claude", maxTokens: 20_000 },
			}),
		).toBe(100_000);
	});
});
