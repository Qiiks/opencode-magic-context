import { describe, expect, it } from "bun:test";
import { MagicContextConfigSchema } from "@magic-context/core/config/schema/magic-context";
import { resolveCacheTtl } from "@magic-context/core/hooks/magic-context/event-resolvers";
import {
	canonicalPiModelKey,
	resolveDreamerFromConfig,
	resolveHistorianFromConfig,
	resolveSidekickFromConfig,
} from "./index";

describe("Pi config resolvers", () => {
	it("resolves a Pi-native cache_ttl key on the canonical model leg", () => {
		const modelKey = canonicalPiModelKey("openai-codex", "gpt-5.6-sol");
		expect(modelKey).toBe("openai/gpt-5.6-sol");
		expect(
			resolveCacheTtl(
				{ default: "5m", "openai-codex/gpt-5.6-sol": "60m" },
				modelKey,
			),
		).toBe("60m");
	});

	it("keeps historian temperature absent unless configuration explicitly sets it", () => {
		const omitted = MagicContextConfigSchema.parse({
			historian: { pi: { model: "test/historian" } },
		});
		const explicit = MagicContextConfigSchema.parse({
			historian: { temperature: 0.1, pi: { model: "test/historian" } },
		});

		expect(resolveHistorianFromConfig(omitted)?.temperature).toBeUndefined();
		expect(resolveHistorianFromConfig(explicit)?.temperature).toBe(0.1);
	});

	it("returns undefined for historian, dreamer, and sidekick when disabled", () => {
		const config = MagicContextConfigSchema.parse({
			historian: { disable: true, model: "test/historian" },
			dreamer: { disable: true, model: "test/dreamer" },
			sidekick: { disable: true, model: "test/sidekick" },
		});

		expect(resolveHistorianFromConfig(config)).toBeUndefined();
		expect(resolveDreamerFromConfig(config)).toBeUndefined();
		expect(resolveSidekickFromConfig(config)).toBeUndefined();
	});
});
