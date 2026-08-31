import { describe, expect, it } from "bun:test";

import { calibrateHistorianProviderPayload } from "./historian-calibration-extension";

describe("historian provider calibration", () => {
	it("sets the calibration triple on every supported output-budget shape", () => {
		const fixtures = [
			{ input: { max_tokens: 4096 }, key: "max_tokens" },
			{ input: { max_completion_tokens: 4096 }, key: "max_completion_tokens" },
			{ input: { max_output_tokens: 4096 }, key: "max_output_tokens" },
			{ input: { maxTokens: 4096 }, key: "maxTokens" },
		] as const;
		for (const fixture of fixtures) {
			const result = calibrateHistorianProviderPayload(
				fixture.input,
				0.1,
				32_000,
			) as Record<string, unknown>;
			expect(result.temperature).toBe(0.1);
			expect(result[fixture.key]).toBe(32_000);
		}
	});

	it("calibrates nested provider generation shapes without adding invalid top-level fields", () => {
		const result = calibrateHistorianProviderPayload(
			{ generationConfig: { topP: 0.9, maxOutputTokens: 4096 } },
			0.1,
			32_000,
		) as Record<string, unknown>;
		expect(result).toEqual({
			generationConfig: {
				topP: 0.9,
				temperature: 0.1,
				maxOutputTokens: 32_000,
			},
		});
		expect(
			calibrateHistorianProviderPayload(
				{ inferenceConfig: { maxTokens: 4096 } },
				0.1,
				32_000,
			),
		).toEqual({ inferenceConfig: { temperature: 0.1, maxTokens: 32_000 } });
	});

	it("preserves an explicit zero temperature", () => {
		expect(calibrateHistorianProviderPayload({ max_tokens: 4096 }, 0, 32_000)).toEqual({
			max_tokens: 32_000,
			temperature: 0,
		});
		expect(
			calibrateHistorianProviderPayload(
				{ generationConfig: { maxOutputTokens: 4096 } },
				0,
				32_000,
			),
		).toEqual({ generationConfig: { maxOutputTokens: 32_000, temperature: 0 } });
	});

	it("never sends temperature when it is not configured", () => {
		const fixtures = [
			{ input: { max_tokens: 4096 }, key: "max_tokens" },
			{ input: { max_completion_tokens: 4096 }, key: "max_completion_tokens" },
			{ input: { max_output_tokens: 4096 }, key: "max_output_tokens" },
			{ input: { maxTokens: 4096 }, key: "maxTokens" },
		] as const;
		for (const fixture of fixtures) {
			const result = calibrateHistorianProviderPayload(
				fixture.input,
				undefined,
				32_000,
			) as Record<string, unknown>;
			expect("temperature" in result).toBe(false);
			expect(result[fixture.key]).toBe(32_000);
		}
	});

	it("omits temperature from nested provider shapes when unconfigured", () => {
		expect(
			calibrateHistorianProviderPayload(
				{ generationConfig: { topP: 0.9, maxOutputTokens: 4096 } },
				undefined,
				32_000,
			),
		).toEqual({ generationConfig: { topP: 0.9, maxOutputTokens: 32_000 } });
		expect(
			calibrateHistorianProviderPayload(
				{ inferenceConfig: { maxTokens: 4096 } },
				undefined,
				32_000,
			),
		).toEqual({ inferenceConfig: { maxTokens: 32_000 } });
	});

	it("applies temperature alone when no output budget is configured", () => {
		const result = calibrateHistorianProviderPayload(
			{ max_tokens: 4096 },
			0.1,
			undefined,
		) as Record<string, unknown>;
		expect(result.temperature).toBe(0.1);
		expect(result.max_tokens).toBe(4096);
	});

	it("returns the payload untouched when neither knob is configured", () => {
		const payload = { max_tokens: 4096 };
		expect(
			calibrateHistorianProviderPayload(payload, undefined, undefined),
		).toEqual({ max_tokens: 4096 });
	});
});
