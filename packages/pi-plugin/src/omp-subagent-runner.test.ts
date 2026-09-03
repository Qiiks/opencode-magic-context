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
	test("surface unavailable → delegates to subprocess fallback instead of failing (never throws)", async () => {
		// With no surface injected and no OMP host (test env), the runner falls
		// back to the subprocess path. The subprocess runner will fail to spawn
		// `pi` in this environment (ENOENT) — the observable contract is a
		// spawn-class failure, NOT a crash, and NOT an "omp surface unavailable"
		// error from the native path.
		const runner = new OmpSubagentRunner({ surface: null });
		const result = await runner.run(BASE_OPTIONS);
		expect(result.ok).toBe(false);
		if (!result.ok) {
			expect(["spawn_failed", "non_zero_exit"]).toContain(result.reason);
			// Delegated: the error comes from the subprocess spawn, not the
			// native surface gate.
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

	test("isLiteralThinkingLevel accepts exactly the wire-level vocabulary", () => {
		for (const level of ["minimal", "low", "medium", "high", "xhigh", "max"]) {
			expect(isLiteralThinkingLevel(level)).toBe(true);
		}
		for (const sentinel of ["off", "auto", "inherit", undefined, "garbage"]) {
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

	test("sentinel thinking levels omit both effort and suffix", async () => {
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
		expect(request.effort).toBeUndefined();
		// openai → openai-codex translation still applies; NO :suffix
		expect(request.model).toBe("openai-codex/gpt-5.4");
	});
});
