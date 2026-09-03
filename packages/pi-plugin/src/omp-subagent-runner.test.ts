import { describe, expect, mock, test } from "bun:test";

import { isLiteralThinkingLevel, OmpSubagentRunner, taskEffortForLevel } from "./omp-subagent-runner";

/**
 * Tests for the OMP-native subagent runner's fail-soft contract and result
 * mapping. The OMP surface is injected (never resolved at runtime here) so
 * the suite is pure — no host modules load.
 */

/** Surface stub returning a canned `StructuredSubagentResult`. */
const FAKE_SETTINGS = {
	init: async () => ({ reloadFromDisk: async () => undefined, get: () => undefined }),
};
const FAKE_AUTH = async () => ({});

function fakeSurface(outcome: {
	result: Record<string, unknown>;
}, capture?: { requests: Array<Record<string, unknown>> }) {
	return {
		runStructuredSubagent: mock(async (request: Record<string, unknown>) => {
			capture?.requests.push(request);
			return outcome as never;
		}),
		Settings: FAKE_SETTINGS,
		discoverAuthStorage: FAKE_AUTH,
	};
}

const BASE_OPTIONS = {
	agent: "historian",
	systemPrompt: "<compartment>… instructions …</compartment>",
	userMessage: "Summarize this chunk.",
	model: "openai/gpt-5.4",
};

describe("OmpSubagentRunner", () => {
	test("surface unavailable → delegates to the fallback runner (hermetic, no pi spawn)", async () => {
		// cubic P2: a real PiSubagentRunner would spawn an actual `pi` child on
		// hosts where Pi/OMP is installed — the test would run a live subagent
		// (or hang, since BASE_OPTIONS carries no timeoutMs). The injected
		// fallback stub makes the delegation hermetic: assert the runner handed
		// the run off (progress marker + stub result) and never touched the
		// native surface path.
		let fallbackCalls = 0;
		const fallbackRunner = {
			harness: "pi",
			run: async (opts: { agent: string }) => {
				fallbackCalls += 1;
				return {
					ok: false as const,
					reason: "spawn_failed" as const,
					error: "stub fallback: pi spawn blocked in test",
					durationMs: 0,
				};
			},
		};
		const progress: Array<{ type: string; argv?: string[] }> = [];
		const runner = new OmpSubagentRunner({
			surface: null,
			fallbackRunner: fallbackRunner as never,
		});
		const result = await runner.run({
			...BASE_OPTIONS,
			onProgress: (event: { type: string; argv?: string[] }) => progress.push(event),
		});
		// Delegation happened exactly once, through the injected stub.
		expect(fallbackCalls).toBe(1);
		// The fallback marker announced the subprocess hand-off.
		expect(progress.some(event => event.argv?.[0] === "omp:fallback-subprocess")).toBe(true);
		expect(result.ok).toBe(false);
		if (!result.ok) {
			// The result came from the stub, not the native surface gate.
			expect(result.error).toContain("stub fallback");
			expect(result.error).not.toContain("omp surface unavailable");
			expect(result.durationMs).toBeGreaterThanOrEqual(0);
		}
	});

	test("exitCode 0 + output → ok:true with assistantText passthrough and meta", async () => {
		const surface = fakeSurface({
			result: {
				exitCode: 0,
				output: "  <compartment><title>t</title></compartment>  ",
				stderr: "",
				truncated: false,
				durationMs: 1234,
				tokens: 4321,
				usage: { input: 100, output: 50, cacheWrite: 0, cacheRead: 0 },
				resolvedModel: "openai/gpt-5.4",
			},
		});
		const runner = new OmpSubagentRunner({ surface: surface as never });
		const result = await runner.run(BASE_OPTIONS);
		expect(result.ok).toBe(true);
		if (result.ok) {
			expect(result.assistantText).toBe("<compartment><title>t</title></compartment>");
			expect(result.meta?.resolvedModel).toBe("openai/gpt-5.4");
			expect(result.meta?.tokens).toBe(4321);
			expect(result.meta?.ompRunner).toBe(true);
		}
	});

	test("non-zero exit with error → mapped failure, transient on rate-limit", async () => {
		const rateLimited = fakeSurface({
			result: {
				exitCode: 1,
				output: "",
				stderr: "429 rate limited",
				truncated: false,
				error: "429 rate limited by provider",
				durationMs: 100,
			},
		});
		const runner = new OmpSubagentRunner({ surface: rateLimited as never });
		const result = await runner.run(BASE_OPTIONS);
		expect(result.ok).toBe(false);
		if (!result.ok) {
			expect(result.reason).toBe("model_failed");
			expect(result.transient).toBe(true);
		}

		const hardFail = fakeSurface({
			result: {
				exitCode: 1,
				output: "",
				stderr: "boom",
				truncated: false,
				error: "boom",
				durationMs: 100,
			},
		});
		const runner2 = new OmpSubagentRunner({ surface: hardFail as never });
		const result2 = await runner2.run(BASE_OPTIONS);
		expect(result2.ok).toBe(false);
		if (!result2.ok) {
			expect(result2.reason).toBe("non_zero_exit");
			expect(result2.transient).toBeUndefined();
		}
	});

	test("empty output with exit 0 → no_assistant so callers try fallback models", async () => {
		const surface = fakeSurface({
			result: { exitCode: 0, output: "", stderr: "", truncated: false, durationMs: 5 },
		});
		const runner = new OmpSubagentRunner({ surface: surface as never });
		const result = await runner.run(BASE_OPTIONS);
		expect(result.ok).toBe(false);
		if (!result.ok) expect(result.reason).toBe("no_assistant");
	});

	test("aborted outcome → abort reason", async () => {
		const surface = fakeSurface({
			result: {
				exitCode: 1,
				output: "",
				stderr: "",
				truncated: false,
				aborted: true,
				error: "aborted by user",
				durationMs: 10,
			},
		});
		const runner = new OmpSubagentRunner({ surface: surface as never });
		const result = await runner.run(BASE_OPTIONS);
		expect(result.ok).toBe(false);
		if (!result.ok) expect(result.reason).toBe("abort");
	});

	test("request shape: assignment carries system role + yield instruction, isolation flags set", async () => {
		const capture: { requests: Array<Record<string, unknown>> } = { requests: [] };
		const surface = fakeSurface(
			{ result: { exitCode: 0, output: "ok", stderr: "", truncated: false, durationMs: 1 } },
			capture,
		);
		const runner = new OmpSubagentRunner({ surface: surface as never });
		await runner.run(BASE_OPTIONS);
		expect(capture.requests).toHaveLength(1);
		const request = capture.requests[0]!;
		expect(request.invocationKind).toBe("task");
		expect(request.agent).toBe("task");
		expect(request.enableIrc).toBe(false);
		expect(request.enableLsp).toBe(false);
		expect(request.keepAlive).toBe(false);
		expect(request.detached).toBe(true);
		const assignment = request.assignment as string;
		expect(assignment).toContain("<system_role>");
		expect(assignment).toContain(BASE_OPTIONS.userMessage);
		expect(assignment).toContain("yield tool");
		// Session isolation knobs.
		const session = request.session as Record<string, unknown>;
		expect(session.enableIrc).toBe(false);
		expect(session.restrictToolNames).toBe(true);
		expect(session.getSessionSpawns).toBeTypeOf("function");
	});

	test("timeoutMs elapsing maps to timeout (fake surface honors signal abort)", async () => {
		const surface = {
			Settings: FAKE_SETTINGS,
			discoverAuthStorage: FAKE_AUTH,
			runStructuredSubagent: mock(
				(request: { signal?: AbortSignal }) =>
					new Promise((_resolve, reject) => {
						request.signal?.addEventListener("abort", () => {
							reject(new DOMException("The operation was aborted.", "AbortError"));
						});
					}),
			),
		};
		const runner = new OmpSubagentRunner({ surface: surface as never });
		const result = await runner.run({ ...BASE_OPTIONS, timeoutMs: 50 });
		expect(result.ok).toBe(false);
		if (!result.ok) expect(result.reason).toBe("timeout");
	});

	test("caller abort → abort reason", async () => {
		const surface = {
			Settings: FAKE_SETTINGS,
			discoverAuthStorage: FAKE_AUTH,
			runStructuredSubagent: mock(
				(request: { signal?: AbortSignal }) =>
					new Promise((_resolve, reject) => {
						if (request.signal?.aborted) {
							reject(new DOMException("The operation was aborted.", "AbortError"));
							return;
						}
						request.signal?.addEventListener("abort", () => {
							reject(new DOMException("The operation was aborted.", "AbortError"));
						});
					}),
			),
		};
		const runner = new OmpSubagentRunner({ surface: surface as never });
		const controller = new AbortController();
		const pending = runner.run({ ...BASE_OPTIONS, timeoutMs: 60_000, signal: controller.signal });
		controller.abort();
		const result = await pending;
		expect(result.ok).toBe(false);
		if (!result.ok) expect(result.reason).toBe("abort");
	});

	test("harness label is omp", () => {
		const runner = new OmpSubagentRunner({ surface: null });
		expect(runner.harness).toBe("omp");
	});
});

describe("thinking level → OMP spawn mapping", () => {
	test("taskEffortForLevel maps literal levels onto positional selectors", () => {
		expect(taskEffortForLevel("minimal")).toBe("lo");
		expect(taskEffortForLevel("low")).toBe("lo");
		expect(taskEffortForLevel("medium")).toBe("med");
		expect(taskEffortForLevel("high")).toBe("hi");
		expect(taskEffortForLevel("xhigh")).toBe("hi");
		expect(taskEffortForLevel("max")).toBe("hi");
	});

	test("taskEffortForLevel omits sentinels and unknowns (OMP default applies)", () => {
		expect(taskEffortForLevel("off")).toBeUndefined();
		expect(taskEffortForLevel("auto")).toBeUndefined();
		expect(taskEffortForLevel("inherit")).toBeUndefined();
		expect(taskEffortForLevel(undefined)).toBeUndefined();
		expect(taskEffortForLevel("garbage")).toBeUndefined();
	});

	test("isLiteralThinkingLevel accepts the wire-level vocabulary plus off", () => {
		for (const level of ["minimal", "low", "medium", "high", "xhigh", "max", "off"]) {
			expect(isLiteralThinkingLevel(level)).toBe(true);
		}
		for (const sentinel of ["auto", "inherit", undefined, "garbage"]) {
			expect(isLiteralThinkingLevel(sentinel as string | undefined)).toBe(false);
		}
	});

	test("configured thinking_level rides both effort and the model-ref suffix", async () => {
		const capture: { requests: Array<Record<string, unknown>> } = { requests: [] };
		const surface = fakeSurface(
			{
				result: {
					exitCode: 0,
					output: "<compartment><title>t</title></compartment>",
					stderr: "",
					truncated: false,
				},
			},
			capture,
		);
		const runner = new OmpSubagentRunner({ surface: surface as never });
		const result = await runner.run({ ...BASE_OPTIONS, thinkingLevel: "max" });
		expect(result.ok).toBe(true);
		const request = capture.requests[0]!;
		// hi: positional selector (validates against OMP's TASK_EFFORTS gate)
		expect(request.effort).toBe("hi");
		// max: exact literal suffix on the ref for per-model precision
		expect(request.model).toBe("openai-codex/gpt-5.4:max");
	});

	test("off rides the model ref as :off (disableReasoning) with effort omitted", async () => {
		const capture: { requests: Array<Record<string, unknown>> } = { requests: [] };
		const surface = fakeSurface(
			{
				result: {
					exitCode: 0,
					output: "<compartment><title>t</title></compartment>",
					stderr: "",
					truncated: false,
				},
			},
			capture,
		);
		const runner = new OmpSubagentRunner({ surface: surface as never });
		await runner.run({ ...BASE_OPTIONS, thinkingLevel: "off" });
		const request = capture.requests[0]!;
		// effort stays omitted: lo/med/hi has no off rung
		expect(request.effort).toBeUndefined();
		// :off parses as a ThinkingLevel suffix and lands as disableReasoning
		expect(request.model).toBe("openai-codex/gpt-5.4:off");
	});

	test("colon-bearing ref with a non-level suffix keeps the id and skips the literal append", async () => {
		const capture: { requests: Array<Record<string, unknown>> } = { requests: [] };
		const surface = fakeSurface(
			{
				result: {
					exitCode: 0,
					output: "<compartment><title>t</title></compartment>",
					stderr: "",
					truncated: false,
				},
			},
			capture,
		);
		const runner = new OmpSubagentRunner({ surface: surface as never });
		// nous-portal/meituan/longcat-2.0:free — :free is part of the model id
		await runner.run({
			...BASE_OPTIONS,
			model: "nous-portal/meituan/longcat-2.0:free",
			thinkingLevel: "max",
		});
		const request = capture.requests[0]!;
		// effort carries the thinking intent instead
		expect(request.effort).toBe("hi");
		// NO invalid :free:max grammar; the id is untouched
		expect(request.model).toBe("nous-portal/meituan/longcat-2.0:free");
	});

	test("ref already carrying a thinking-level suffix gets it replaced", async () => {
		const capture: { requests: Array<Record<string, unknown>> } = { requests: [] };
		const surface = fakeSurface(
			{
				result: {
					exitCode: 0,
					output: "<compartment><title>t</title></compartment>",
					stderr: "",
					truncated: false,
				},
			},
			capture,
		);
		const runner = new OmpSubagentRunner({ surface: surface as never });
		// config entry written in selector form — the configured level wins
		await runner.run({
			...BASE_OPTIONS,
			model: "bai/glm-5.3-flash:high",
			thinkingLevel: "max",
		});
		const request = capture.requests[0]!;
		expect(request.effort).toBe("hi");
		expect(request.model).toBe("bai/glm-5.3-flash:max");
	});
	test("off on a :free ref delegates to the subprocess runner (structured path cannot express it)", async () => {
		const capture: { requests: Array<Record<string, unknown>> } = { requests: [] };
		const surface = fakeSurface(
			{
				result: {
					exitCode: 0,
					output: "<compartment><title>t</title></compartment>",
					stderr: "",
					truncated: false,
				},
			},
			capture,
		);
		let fallbackCalls = 0;
		const fallbackRunner = {
			harness: "pi",
			run: async () => {
				fallbackCalls += 1;
				return { ok: true as const, assistantText: "fallback-ok", durationMs: 1 };
			},
		};
		const runner = new OmpSubagentRunner({
			surface: surface as never,
			fallbackRunner: fallbackRunner as never,
		});
		const result = await runner.run({
			...BASE_OPTIONS,
			model: "nous-portal/meituan/longcat-2.0:free",
			thinkingLevel: "off",
		});
		// Delegated — the structured path has no disable-reasoning channel for
		// a non-level-suffixed ref, so the subprocess runner (--thinking off)
		// serves this spawn.
		expect(fallbackCalls).toBe(1);
		// The native surface was never invoked.
		expect(capture.requests).toHaveLength(0);
		expect(result.ok).toBe(true);
		if (result.ok) expect(result.assistantText).toBe("fallback-ok");
	});
});
