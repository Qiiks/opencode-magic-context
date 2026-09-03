/**
 * OMP-native subagent runner.
 *
 * Implements the harness-agnostic `SubagentRunner` contract on top of OMP's
 * in-process structured-subagent machinery (`runStructuredSubagent`) instead
 * of the headless `--print` subprocess path used on plain Pi. See
 * cortexkit/magic-context#416: spawning through OMP's task machinery makes
 * MC subagents visible in the OMP subagent pane / Agent Hub while keeping
 * the main agent isolated (per-spawn `enableIrc: false` removes the child's
 * hub tool entirely, and an MC-initiated spawn is neither an owner job nor a
 * `parentId: Main` registry child the model can cancel).
 *
 * In-process by design — mirrors OMP's own Cleanse feature, which builds a
 * synthetic `ToolSession` and calls `runStructuredSubagent` directly
 * (`coding-agent/src/cleanse/agent.ts`). The `--print` subprocess model
 * cannot reach the pane/event-bus surfaces.
 *
 * Result mapping follows the `SubagentRunner` fail-soft contract: transient
 * errors, timeouts, aborts, and model failures surface as
 * `{ ok: false, reason }`; throwing is reserved for programmer errors.
 */

import {
	resolveModelRefForOmp,
} from "@magic-context/core/shared/harness-provider-map";
import type {
	SubagentProgressEvent,
	SubagentRunOptions,
	SubagentRunResult,
	SubagentRunner,
} from "@magic-context/core/shared/subagent-runner";
import { recordChildInvocation } from "@magic-context/core/features/magic-context/subagent-token-capture";
import { openDatabase } from "@magic-context/core/features/magic-context/storage";
import { inferAccountingSubagent, PiSubagentRunner } from "./subagent-runner";
import { loadOmpSubagentSurface, type OmpSubagentSurface } from "./omp-host";

/**
 * Subprocess fallback for OMP hosts whose structured-subagent surface cannot
 * load (cortexkit/magic-context#418 review: a detected OMP process must never
 * dead-end the historian when the subprocess path is available). Constructed
 * lazily on first need; `undefined` until then.
 */
let subprocessFallback: PiSubagentRunner | undefined;

/** Terminator instruction appended to every OMP assignment (yield contract). */
const OMP_YIELD_INSTRUCTION =
	"When you have finished the task, call the yield tool with your complete final output verbatim — do not summarize or reformat it.";

/** Default wall-clock cap when the caller does not set `timeoutMs`. */
const DEFAULT_TIMEOUT_MS = 10 * 60 * 1000;

/** Live host pieces the caller captured from the OMP extension context. */
export interface OmpRunnerHostContext {
	/** Canonical `provider/model` id (ctx.model), when the session has one. */
	model?: string | undefined;
	/** Live host session file, when known (falls back to a temp artifacts lease). */
	sessionFile?: string | null;
	/** Live host session id, when known. */
	sessionId?: string | null;
	/**
	 * The host session's live ModelRegistry. OMP's structured spawn path
	 * forwards `session.modelRegistry` into `runSubprocess`
	 * (structured-subagent.ts:433 → executor.ts `registryFromParent`), so
	 * passing the parent's registry here makes extension-registered runtime
	 * providers (bai, nous-portal, kiosapi, …) resolvable in the child.
	 * Without it, `runSubprocess` constructs a fresh registry that only knows
	 * bundled-catalog providers + models.yml discoverables — a configured
	 * historian model from a runtime provider then fails to resolve and the
	 * spawn silently falls back to the session default ("No model selected"
	 * when even that fails).
	 */
	modelRegistry?: unknown;
}

export interface OmpSubagentRunnerOptions {
	/** Test seam: inject a pre-loaded surface instead of runtime resolution. */
	surface?: OmpSubagentSurface | null;
	/** Test seam: override the host context snapshot. */
	hostContext?: OmpRunnerHostContext;
	/**
	 * Test seam: replace the subprocess fallback used when the OMP surface is
	 * unavailable. Production constructs a real PiSubagentRunner lazily; tests
	 * inject a stub so the delegation path is hermetic (no `pi` binary spawn,
	 * which on hosts with Pi installed would run a live subagent or hang).
	 */
	fallbackRunner?: SubagentRunner;
}

/**
 * Build the minimal synthetic ToolSession the structured spawn path needs.
 * Only `cwd`, `hasUI`, `getSessionFile`, `getSessionSpawns`, and `settings`
 * are required by OMP's type; the rest of the executor wiring reads the
 * optional accessors it finds and degrades gracefully when absent.
 */
function buildToolSession(
	cwd: string,
	host: OmpRunnerHostContext,
	settings: unknown,
	authStorage: unknown,
): Record<string, unknown> {
	return {
		cwd,
		hasUI: false,
		// Hard isolation: no hub tool on the child, no IRC roster, no steering.
		enableIrc: false,
		// The historian is a pure summarizer — no LSP, no MCP.
		enableLsp: false,
		enableMCP: false,
		restrictToolNames: true,
		taskDepth: 0,
		// The spawn-policy gate (`assertDepthAndSpawnAllowed`) reads this string.
		getSessionSpawns: () => "task",
		getAgentId: () => null,
		getSessionFile: () => host.sessionFile ?? null,
		getSessionId: () => host.sessionId ?? null,
		getModelString: () => host.model,
		getActiveModelString: () => host.model,
		// The historian is MC-internal, not a user-visible spawn announcement.
		suppressSpawnAdvisory: true,
		settings,
		authStorage,
		// Parent's live registry — lets the structured spawn resolve
		// extension-registered (runtime) providers instead of falling back to
		// a fresh registry that only knows bundled + models.yml providers.
		modelRegistry: host.modelRegistry,
	};
}

/** Compose the assignment: historian persona (as system_role) + task + yield contract. */
function buildAssignment(systemPrompt: string, userMessage: string): string {
	const sections: string[] = [];
	if (systemPrompt.trim().length > 0) {
		sections.push(
			`<system_role>\n${systemPrompt.trim()}\n</system_role>`,
			"Treat the <system_role> block above as your complete operating instructions for this run.",
		);
	}
	sections.push(userMessage, OMP_YIELD_INSTRUCTION);
	return sections.join("\n\n");
}

/** Map an OMP error message onto the shared runner failure taxonomy. */
function mapFailureReason(
	error: string,
	aborted: boolean,
	timedOut: boolean,
): SubagentRunResult extends { ok: false; reason: infer R } ? R : never {
	if (timedOut) return "timeout" as never;
	if (aborted) return "abort" as never;
	if (/rate.?limit|429|quota|capacity|overloaded/i.test(error)) {
		return "model_failed" as never;
	}
	return "non_zero_exit" as never;
}

function sanitizeLabel(value: string): string {
	return value.replace(/[^a-zA-Z0-9._-]/g, "").slice(0, 48) || "Subagent";
}

/**
 * Thinking selectors OMP's model-pattern grammar accepts as `:level`
 * suffixes (parseThinkingLevel vocabulary — the same table that backs
 * `--thinking`). Includes `off`, which is a wire-real selector: it parses
 * as a `:off` suffix and lands on the child as `disableReasoning: true`
 * (shouldDisableReasoning). `auto`/`inherit` are NOT included — `auto` is
 * gated out of suffixes by parseThinkingSuffix (only via allowAutoAlias)
 * and `inherit` is a role-storage sentinel, not a spawn selector.
 */
const LITERAL_THINKING_LEVEL: Record<string, true> = {
	minimal: true,
	low: true,
	medium: true,
	high: true,
	xhigh: true,
	max: true,
	off: true,
};

/** True when the configured level can ride a model ref as an exact `:level` suffix. */
export function isLiteralThinkingLevel(level: string | undefined): level is string {
	return level !== undefined && LITERAL_THINKING_LEVEL[level] === true;
}

/**
 * Strip a trailing `:suffix` from a model ref when that suffix parses as a
 * thinking level (e.g. a config entry already written in selector form,
 * `bai/glm-5.3-flash:high`). A non-level suffix (`nous-portal/
 * meituan/longcat-2.0:free`) is left intact — it is part of the model id,
 * and appending would produce the invalid grammar `:free:max`.
 */
function stripThinkingSuffixFromRef(ref: string): string {
	const colonIdx = ref.lastIndexOf(":");
	if (colonIdx <= 0) return ref;
	const suffix = ref.slice(colonIdx + 1);
	// Reuse OMP's own level vocabulary: anything the selector table knows is
	// a thinking suffix, anything else is part of the id.
	if (LITERAL_THINKING_LEVEL[suffix] === true) return ref.slice(0, colonIdx);
	return ref;
}

/**
 * Map a literal thinking level onto OMP's positional TaskEffort selector.
 * OMP's task-spawn surface only accepts "lo" | "med" | "hi"
 * (TASK_EFFORTS, thinking.ts:270) — validateEffort rejects anything else —
 * and resolveTaskEffortLevel then maps the selector onto the target model's
 * OWN supported ladder (lo = lowest supported, med = middle, hi = highest,
 * which is xhigh or max on models that go that high).
 *
 *   minimal/low -> "lo"   medium -> "med"   high/xhigh/max -> "hi"
 *   off/auto/inherit/undefined -> undefined (omit; OMP default applies)
 *
 * Long ladders lose precision here (e.g. "medium" on a 6-rung ladder lands
 * between rungs), which is why the literal `:level` suffix rides the model
 * ref alongside — the selector is the fallback, not the primary signal.
 */
export function taskEffortForLevel(level: string | undefined): "lo" | "med" | "hi" | undefined {
	switch (level) {
		case "minimal":
		case "low":
			return "lo";
		case "medium":
			return "med";
		case "high":
		case "xhigh":
		case "max":
			return "hi";
		default:
			return undefined;
	}
}

export class OmpSubagentRunner implements SubagentRunner {
	readonly harness = "omp";

	private readonly injectedSurface: OmpSubagentSurface | null | undefined;
	private hostContext: OmpRunnerHostContext;
	private readonly injectedFallback: SubagentRunner | undefined;

	constructor(options: OmpSubagentRunnerOptions = {}) {
		this.injectedSurface = options.surface;
		this.hostContext = options.hostContext ?? {};
		this.injectedFallback = options.fallbackRunner;
	}

	/**
	 * Update the live host context snapshot (model ref, session ids, and the
	 * parent's ModelRegistry). Called by the extension when it has a live
	 * `ExtensionContext` in scope — the singleton runner is constructed
	 * before any session exists, so construction-time capture would be empty.
	 */
	setHostContext(patch: Partial<OmpRunnerHostContext>): void {
		this.hostContext = { ...this.hostContext, ...patch };
	}

	/**
	 * Resolve the surface lazily on first run (construction must not touch
	 * host modules — the runner may be constructed on non-OMP hosts where the
	 * import would fail).
	 */
	private async resolveSurface(): Promise<OmpSubagentSurface | { error: string }> {
		if (this.injectedSurface !== undefined) {
			return this.injectedSurface ?? { error: "injected surface unavailable" };
		}
		const loaded = await loadOmpSubagentSurface();
		if (loaded.surface) return loaded.surface;
		return { error: loaded.reason ?? "omp surface unavailable" };
	}

	async run(options: SubagentRunOptions): Promise<SubagentRunResult> {
		const startedAt = Date.now();
		const duration = () => Date.now() - startedAt;
		// Snapshot the live host context for this run: session_start can fire
		// while a run is awaiting settings/auth init, and the singleton must
		// not build the in-flight run with the new session's registry and ids.
		const host = { ...this.hostContext };
		// Hoisted so the outer catch can distinguish deadline aborts from
		// caller aborts (the AbortError message alone cannot).
		let timedOut = false;
		// Accounting parity with PiSubagentRunner.runOnce: best-effort
		// subagent_invocations row so /ctx-status and token totals see OMP
		// runs the same way they see subprocess runs. Failure to record must
		// never fail the run.
		const recordAccounting = (result: SubagentRunResult, usage?: { input?: number; output?: number; cacheWrite?: number; cacheRead?: number }) => {
			if (!options.accountingSessionId) return;
			try {
				recordChildInvocation({
					db: openDatabase(),
					parentSessionId: options.accountingSessionId,
					harness: "omp",
					subagent: options.accountingSubagent ?? inferAccountingSubagent(options.agent),
					task: options.accountingTask ?? null,
					startedAt,
					endedAt: Date.now(),
					status: result.ok ? "completed" : result.reason === "abort" ? "aborted" : "failed",
					providerId: typeof options.model === "string" ? options.model.split("/")[0] : null,
					modelId: typeof options.model === "string" ? options.model.split("/").slice(1).join("/") : null,
					tokens: usage
						? {
								input: usage.input ?? 0,
								output: usage.output ?? 0,
								cacheRead: usage.cacheRead ?? 0,
								cacheWrite: usage.cacheWrite ?? 0,
							}
						: undefined,
					error: result.ok ? null : result.error,
					parentInvocationId: options.accountingParentInvocationId ?? null,
				});
			} catch {
				// Best-effort: never fail the run on accounting.
			}
		};
		try {
			const resolved = await this.resolveSurface();
			if (!("runStructuredSubagent" in resolved)) {
				// Surface unavailable on a detected OMP host — delegate to the
				// subprocess runner instead of failing the run. The fallback is
				// sticky for the process: if the surface cannot load once (broken
				// install, missing package), it will not load later.
				// Tests inject `fallbackRunner` so this delegation is hermetic —
				// a real PiSubagentRunner would spawn an actual `pi` child on
				// hosts where Pi is installed (hangs without a timeout).
				const fallback = this.injectedFallback ?? (subprocessFallback ??= new PiSubagentRunner());
				options.onProgress?.({
					type: "spawned",
					argv: ["omp:fallback-subprocess", options.agent],
					pid: undefined,
				});
				return fallback.run(options);
			}
			const surface = resolved;

			const cwd = options.cwd ?? process.cwd();
		// OMP selects models by its own selector grammar; the canonical
		// provider prefix may need translation (openai→openai-codex etc.),
		// which resolveModelRefForOmp already implements for the subprocess
		// path. The explicitly configured model (options.model) wins; the
		// host's ambient session model is only the fallback when nothing is
		// configured — reversing this would route every historian run to the
		// parent's model instead of the configured historian model.
		// `host` is the run-start snapshot (see top of run()).
		const hostModel = options.model ?? host.model;
		const modelRef = hostModel ? resolveModelRefForOmp(hostModel) : undefined;

			// Timeout: race the caller's signal against an internal deadline.
			const timeoutMs = options.timeoutMs ?? DEFAULT_TIMEOUT_MS;
			const controller = new AbortController();
			const abortFromCaller = () => controller.abort();
			options.signal?.addEventListener("abort", abortFromCaller, { once: true });
			// A signal that aborted before this line never fires its event —
			// honor the already-aborted state immediately.
			if (options.signal?.aborted) abortFromCaller();
			const timer = setTimeout(() => {
				timedOut = true;
				controller.abort();
			}, timeoutMs);
			try {
				const settings = await surface.Settings.init({ cwd });
				const authStorage = await surface.discoverAuthStorage();
				const session = buildToolSession(cwd, host, settings, authStorage);
				// Thinking for this spawn, honoring the config's `thinking_level`
				// (`options.thinkingLevel`), which the subprocess path enforces
				// via `--thinking`. Two OMP-sanctioned mechanisms:
				//
				// 1. `effort` — the positional TaskEffort selector ("lo" | "med" |
				//    "hi"). resolveTaskEffortLevel maps it onto the target model's
				//    OWN supported ladder (lo = lowest supported, hi = whatever the
				//    model tops out at: high/xhigh/max). Literal levels ("high",
				//    "max") are NOT valid here — OMP's validateEffort rejects
				//    anything outside lo/med/hi.
				// 2. A `:level` suffix on the model ref — OMP's model-pattern
				//    grammar (splitThinkingSuffix) accepts exact literal levels
				//    (minimal/low/medium/high/xhigh/max, plus `off` for
				//    disableReasoning) and resolves them as explicitThinkingLevel.
				//    Exact per-model; effort is the positional fallback for long
				//    ladders where the selector is less precise.
				const effort = taskEffortForLevel(options.thinkingLevel);
				const literalLevel = isLiteralThinkingLevel(options.thinkingLevel)
					? options.thinkingLevel
					: undefined;
				let model: string | undefined;
				// The one combination the structured path cannot express: `off`
				// (disable reasoning) on a ref whose trailing suffix is part of
				// the model id (`:free`). Appending is invalid grammar
				// (`:free:off`) and there is no effort rung for "off" — so the
				// subprocess runner, which passes `--thinking off`, serves this
				// spawn instead.
				const offBlockedByNonLevelSuffix =
					options.thinkingLevel === "off" &&
					modelRef !== undefined &&
					stripThinkingSuffixFromRef(modelRef) === modelRef &&
					modelRef.lastIndexOf(":") > 0;
				if (offBlockedByNonLevelSuffix) {
					const fallback = this.injectedFallback ?? (subprocessFallback ??= new PiSubagentRunner());
					options.onProgress?.({
						type: "spawned",
						argv: ["omp:fallback-subprocess", options.agent],
						pid: undefined,
					});
					return fallback.run(options);
				}
				if (modelRef !== undefined) {
					if (literalLevel === undefined) {
						model = modelRef;
					} else {
						const stripped = stripThinkingSuffixFromRef(modelRef);
						const hadThinkingSuffix = stripped !== modelRef;
						const refHasNonLevelSuffix = modelRef.lastIndexOf(":") > 0 && !hadThinkingSuffix;
						if (refHasNonLevelSuffix) {
							model = modelRef;
						} else {
							model = `${stripped}:${literalLevel}`;
						}
					}
				}
				const request = {
					session,
					invocationKind: "task" as const,
					assignment: buildAssignment(options.systemPrompt, options.userMessage),
					agent: "task",
					...(effort !== undefined ? { effort } : {}),
					...(model !== undefined ? { model } : {}),
					identity: { label: `MagicContext-${sanitizeLabel(options.agent)}` },
					enableIrc: false,
					enableLsp: false,
					keepAlive: false,
					detached: true,
					signal: controller.signal,
				};
				options.onProgress?.({ type: "spawned", argv: ["omp:structured-subagent", options.agent], pid: undefined });
				const outcome = await surface.runStructuredSubagent(request);
				const single = outcome.result;
				options.onProgress?.({ type: "first_event", eventType: "settled", ms: duration() });

				const usage = single.usage;
				const successResult: SubagentRunResult =
					single.exitCode === 0 && !single.error && single.aborted !== true
						? { ok: true, assistantText: single.output.trim(), durationMs: duration() }
					: (() => {
							const reason = mapFailureReason(
								single.error ?? "omp spawn failed",
								single.aborted === true,
								controller.signal.aborted && timedOut,
							);
							return {
								ok: false as const,
								reason,
								error: single.error ?? "omp spawn failed",
								durationMs: duration(),
								// Rate-limit/capacity classes are retryable — mirror the
								// subprocess runner's transient hint for fallback chains.
								...(reason === "model_failed" ? { transient: true } : {}),
							};
						})();
				// Record AFTER final classification: an exit-0 run with empty
				// output is a failed attempt (no_assistant), not a completed
				// invocation — recording the pre-classification result would log
				// `completed` for a run that returns no_assistant.
				if (!successResult.ok) {
					recordAccounting(successResult, usage);
					return successResult;
				}
				if (successResult.assistantText.length === 0) {
					const finalResult: SubagentRunResult = {
						ok: false,
						reason: "no_assistant",
						error: "omp structured spawn produced no output",
						durationMs: duration(),
					};
					recordAccounting(finalResult, usage);
					return finalResult;
				}
				recordAccounting(successResult, usage);
				return {
					ok: true,
					assistantText: successResult.assistantText,
					durationMs: successResult.durationMs,
					meta: {
						resolvedModel: single.resolvedModel,
						tokens: single.tokens,
						truncated: single.truncated === true,
						ompRunner: true,
					},
				};
			} finally {
				clearTimeout(timer);
				options.signal?.removeEventListener("abort", abortFromCaller);
			}
		} catch (error) {
			// StructuredSubagentError preflight failures (unknown agent, spawn
			// policy, isolation) and any unexpected host error land here.
			const message = error instanceof Error ? error.message : String(error);
			const progress: SubagentProgressEvent = {
				type: "child_exit",
				code: 1,
				signal: null,
				ms: duration(),
			};
			options.onProgress?.(progress);
			// Every terminal failure records accounting — a rejected preflight
			// or deadline abort is still a historian attempt the invocation
			// history and token aggregates must not omit.
			if (timedOut) {
				const result: SubagentRunResult = {
					ok: false,
					reason: "timeout",
					error: `historian run exceeded deadline: ${message}`,
					durationMs: duration(),
				};
				recordAccounting(result);
				return result;
			}
			if (/aborted|AbortSignal/i.test(message)) {
				const result: SubagentRunResult = { ok: false, reason: "abort", error: message, durationMs: duration() };
				recordAccounting(result);
				return result;
			}
			const result: SubagentRunResult = { ok: false, reason: "spawn_failed", error: message, durationMs: duration() };
			recordAccounting(result);
			return result;
		}
	}
}
