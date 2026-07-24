/// <reference types="bun-types" />

import { afterAll, beforeAll, describe, expect, it } from "bun:test";
import { RustTestHarness, type RustPassLine } from "../src/rust-harness";
import { rustPrereqs } from "../src/rust-scenario-support";

const formatTiming = (pass: RustPassLine) => ({
	messages: pass.inputCount,
	adapter_ms: pass.adapterElapsedMs,
	module_ms: pass.moduleElapsedMs,
	prefix_guard_ms: pass.prefixGuardMs,
	state_sync_ms: pass.stateSyncMs,
	wire_build_ms: pass.wireBuildMs,
	wire_messages: pass.wireMessages,
	transport_ms: pass.transportMs,
	transport_pages: pass.transportPages,
	transport_bytes: pass.transportBytes,
});

describe.skipIf(!rustPrereqs.ok)(
	"rust transport: multi-frame tail delta",
	() => {
		let h: RustTestHarness;

		beforeAll(async () => {
			h = await RustTestHarness.create({
				modelContextLimit: 50_000_000,
				magicContextConfig: {
					execute_threshold_percentage: 95,
					protected_tags: 1,
					compressor: { enabled: false },
				},
			});
		});

		afterAll(async () => {
			await h?.dispose();
		});

		it("pages only the changed tail of a 1,000-message session", async () => {
			const sessionId = await h.createSession();
			await h.sendPrompt(sessionId, "establish the initial module snapshot");
			await h.waitForRustPasses(1);

			h.appendSyntheticHistory(sessionId, { count: 1_000, textBytes: 1024 });
			await h.restart({
				rust: true,
				magicContextConfig: {
					execute_threshold_percentage: 95,
					protected_tags: 1,
					compressor: { enabled: false },
				},
			});
			await h.sendPrompt(
				sessionId,
				"prime the synthetic big-session snapshot",
				{
					timeoutMs: 300_000,
				},
			);
			const primed = await h.waitFor(
				() => {
					const passes = h.readRustPasses();
					for (let index = passes.length - 1; index >= 0; index -= 1) {
						if (passes[index]!.inputCount > 1_000) return passes[index];
					}
					return undefined;
				},
				{ timeoutMs: 30_000, label: "primed 1,000-message rust pass" },
			);
			expect(primed.inputCount).toBeGreaterThan(1_000);
			expect(primed.applied).toBe(true);

			const smallDeltas: RustPassLine[] = [];
			for (let probe = 0; probe < 5; probe += 1) {
				const before = h.readRustPasses().length;
				await h.sendPrompt(sessionId, `small steady-state delta ${probe}`);
				smallDeltas.push((await h.waitForRustPasses(before + 1)).at(-1)!);
			}
			const smallDelta = [...smallDeltas].sort(
				(left, right) => left.adapterElapsedMs - right.adapterElapsedMs,
			)[Math.floor(smallDeltas.length / 2)]!;

			const before = h.readRustPasses().length;
			await h.sendPrompt(
				sessionId,
				`multi-frame tail delta: ${h.ballast(160_000)}`,
				{
					timeoutMs: 300_000,
				},
			);
			const multiFrameDelta = (await h.waitForRustPasses(before + 1)).at(-1)!;

			console.log(
				`[rust-e2e] multi-frame delta timings ${JSON.stringify({
					primed: formatTiming(primed),
					small_delta_samples: smallDeltas.map(formatTiming),
					small_delta_p50: formatTiming(smallDelta),
					multi_frame_delta: formatTiming(multiFrameDelta),
				})}`,
			);

			expect(smallDeltas.every((pass) => pass.applied)).toBe(true);
			expect(smallDelta.prefixGuardMs).toBeLessThan(10);
			expect(smallDelta.stateSyncMs).toBeLessThan(15);
			expect(smallDelta.wireBuildMs).toBeLessThan(10);
			// The hermetic daemon connects ck-mc as an external TCP provider and can add a
			// fixed scheduling delay. Production-like transport substrates opt into the
			// wall-clock gate while every environment enforces the payload-size invariants.
			if (process.env.MC_RUST_E2E_STRICT_PERF === "1") {
				expect(smallDelta.transportMs).toBeLessThan(30);
				expect(smallDelta.adapterElapsedMs).toBeLessThan(100);
			}
			expect(smallDeltas.every((pass) => pass.wireMessages <= 4)).toBe(true);
			expect(smallDeltas.every((pass) => pass.transportPages === 1)).toBe(true);
			expect(
				smallDeltas.every((pass) => pass.transportBytes < 512 * 1024),
			).toBe(true);
			expect(multiFrameDelta.applied).toBe(true);
			expect(multiFrameDelta.transportPages).toBeGreaterThan(1);
			expect(multiFrameDelta.transportPages).toBeLessThanOrEqual(6);
			expect(multiFrameDelta.wireMessages).toBeLessThanOrEqual(4);
		}, 600_000);
	},
);
