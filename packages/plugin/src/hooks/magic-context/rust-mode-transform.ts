import { resolveProjectIdentity } from "../../features/magic-context/memory/project-identity";
import type { getOrCreateSessionMeta } from "../../features/magic-context/storage";
import { casChannel2NudgeState } from "../../features/magic-context/storage-meta-persisted";
import type { ContextUsage } from "../../features/magic-context/types";
import { sessionLog } from "../../shared/logger";
import { maybeDeliverChannel2 } from "./channel2-delivery";
import {
    resolveExecuteThreshold,
    resolveModelKey,
    resolveTrustedContextLimit,
} from "./event-resolvers";
import {
    type ModuleCompartmentMirrorResponse,
    type ModuleCompartmentReader,
    type ModuleStateSyncClient,
    type ModuleStateSyncState,
    mirrorModuleCompartments,
    syncModuleState,
} from "./module-state-sync";
import {
    buildPagedModuleTransformPayloads,
    encodeOpenCodeMessagesToCk,
    resolveOrdinalsForModule,
} from "./module-wire";
import { findLastAssistantModelFromOpenCodeDb, isMidTurn } from "./read-session-db";
import type { RawMessageOrdinalAnchor } from "./read-session-raw";
import type { TransformDeps } from "./transform";
import { resolveHistoryBudgetTokens } from "./transform";
import { loadContextUsage } from "./transform-context-state";
import type { MessageLike } from "./transform-operations";

const RUST_FAILURE_PARK_THRESHOLD = 3;
const RUST_PROBE_INTERVAL = 5;
const RUST_SEND_TIMEOUT_MS = 15_000;

export interface RustModeModuleClient extends ModuleStateSyncClient {
    closeSession?(sessionId: string): void;
    getCompartmentsAfter?(
        sessionId: string,
        afterSequence: number,
    ): Promise<ModuleCompartmentMirrorResponse>;
}

interface RustSessionState extends ModuleStateSyncState {
    initialized: boolean;
    consecutiveFailures: number;
    passCount: number;
    parked: boolean;
    passesSincePark: number;
    warningSent: boolean;
    ordinalMemoAnchor: RawMessageOrdinalAnchor | null;
    ordinalMemoStoredCount: number | null;
    ordinalMemoCanonicalCount: number;
    failureCount: number;
    parkCount: number;
}

export interface RustModeTransformOptions {
    moduleClient: RustModeModuleClient;
    hostClient?: unknown;
    projectRoot?: string;
    notifyParked?: (sessionId: string, message: string) => void;
    moduleTimeoutMs?: number;
}

function cloneForModule<T>(value: T): T {
    return JSON.parse(JSON.stringify(value)) as T;
}

function isRecord(value: unknown): value is Record<string, unknown> {
    return value !== null && typeof value === "object";
}

function responseValue(response: unknown): Record<string, unknown> {
    if (isRecord(response) && isRecord(response.result)) return response.result;
    if (isRecord(response)) return response;
    throw new Error("module transform returned a non-object response");
}

function modelFromMessages(
    messages: MessageLike[],
): { providerID: string; modelID: string } | undefined {
    for (let index = messages.length - 1; index >= 0; index -= 1) {
        const info = messages[index]?.info as Record<string, unknown> | undefined;
        const model = isRecord(info?.model) ? info.model : undefined;
        if (typeof model?.providerID === "string" && typeof model.modelID === "string") {
            return { providerID: model.providerID, modelID: model.modelID };
        }
        if (
            typeof info?.providerID === "string" &&
            typeof info.modelID === "string" &&
            info.role === "assistant"
        ) {
            return { providerID: info.providerID, modelID: info.modelID };
        }
    }
    return undefined;
}

function ensureState(states: Map<string, RustSessionState>, sessionId: string): RustSessionState {
    let state = states.get(sessionId);
    if (!state) {
        state = {
            initialized: false,
            consecutiveFailures: 0,
            passCount: 0,
            parked: false,
            passesSincePark: 0,
            warningSent: false,
            ordinalMemoAnchor: null,
            ordinalMemoStoredCount: null,
            ordinalMemoCanonicalCount: 0,
            seedPassPending: true,
            failureCount: 0,
            parkCount: 0,
            shadowGeneration: 0,
            lastAckedSeq: 0,
            lastAckedWatermarks: null,
            idOrdinalMemoGeneration: 0,
            idOrdinalMemo: new Map(),
        };
        states.set(sessionId, state);
    }
    return state;
}

function getSessionDirectory(
    deps: TransformDeps,
    sessionId: string,
): Promise<{ directory: string; resolvedFromHost: boolean }> {
    const cached = deps.sessionDirectoryBySession?.get(sessionId);
    if (cached) return Promise.resolve({ directory: cached, resolvedFromHost: true });
    if (!deps.client)
        return Promise.resolve({
            directory: deps.directory ?? process.cwd(),
            resolvedFromHost: false,
        });
    return Promise.resolve().then(async () => {
        try {
            const response = await deps.client?.session
                ?.get({ path: { id: sessionId } })
                .catch(() => null);
            const directory = (response as { data?: { directory?: unknown } } | null)?.data
                ?.directory;
            if (typeof directory === "string" && directory.length > 0) {
                deps.sessionDirectoryBySession?.set(sessionId, directory);
                return { directory, resolvedFromHost: true };
            }
        } catch {
            // The launch directory is a safe non-fatal fallback for module routing.
        }
        return { directory: deps.directory ?? process.cwd(), resolvedFromHost: false };
    });
}

function passUsage(usage: ContextUsage, limit: number): Record<string, number> {
    return {
        input_tokens: usage.inputTokens,
        limit,
        current_total_input_tokens: usage.inputTokens,
        context_limit_tokens: limit,
    };
}

function directiveTextOf(response: Record<string, unknown>): string | undefined {
    const directives = isRecord(response.host_directives) ? response.host_directives : undefined;
    const channel2 = isRecord(directives?.channel2_nudge) ? directives.channel2_nudge : undefined;
    return typeof channel2?.text === "string" && channel2.text.length > 0
        ? channel2.text
        : undefined;
}

function isNeedFullSync(response: Record<string, unknown>): boolean {
    return response.status === "need_full_sync" || response.action === "NEED_FULL_SYNC";
}

/** Single response-field seam for the parallel module encode-back contract. */
export function applyNativeMessagesVerbatim(
    output: { messages: unknown[] },
    response: Record<string, unknown>,
): void {
    const nativeMessages = response.native_messages;
    if (!Array.isArray(nativeMessages)) {
        throw new Error("rust transform response omitted native_messages");
    }
    // The module owns healing, ordering, and codec fidelity. Do not clone,
    // normalize, or otherwise inspect the returned native message array.
    output.messages = nativeMessages;
}

function buildTransformBody(args: {
    sessionId: string;
    input: unknown[];
    nativeMessages: unknown[];
    passInputs: Record<string, unknown>;
    usage: Record<string, number>;
    modelKey: string | null;
    midTurn: boolean;
    declaredTrim?: unknown;
}): Record<string, unknown> {
    return {
        method: "transform",
        kind: "transform",
        v: 2,
        serializer_profile: "opencode-aisdk",
        serve_native: true,
        session_id: args.sessionId,
        render_config: "",
        messages: args.input,
        native_messages: args.nativeMessages,
        usage: args.usage,
        provider_error: args.passInputs.provider_error,
        mid_turn: args.midTurn,
        model_key: args.modelKey,
        effective_execute_threshold: args.passInputs.effective_execute_threshold,
        history_budget_tokens: args.passInputs.history_budget_tokens,
        cache_ttl: args.passInputs.cache_ttl,
        is_subagent: args.passInputs.is_subagent,
        pass_inputs: args.passInputs,
        declared_trim: args.declaredTrim,
    };
}

export function createRustModeTransform(
    deps: TransformDeps,
    options: RustModeTransformOptions,
): {
    run: (
        sessionId: string,
        messages: MessageLike[],
        output: { messages: unknown[] },
        sessionMeta: ReturnType<typeof getOrCreateSessionMeta>,
    ) => Promise<void>;
    clearSession: (sessionId: string) => void;
    getState: (sessionId: string) => Readonly<RustSessionState>;
} {
    const states = new Map<string, RustSessionState>();
    const timeoutMs = Math.max(1, options.moduleTimeoutMs ?? RUST_SEND_TIMEOUT_MS);

    const callModule = async (
        args: Parameters<RustModeModuleClient["call"]>[0],
    ): Promise<unknown> => {
        const controller = new AbortController();
        const timer = setTimeout(
            () => controller.abort(new Error("rust module request timed out")),
            timeoutMs,
        );
        try {
            return await options.moduleClient.call({ ...args, signal: controller.signal });
        } finally {
            clearTimeout(timer);
        }
    };

    const markFailure = (sessionId: string, state: RustSessionState, error: unknown): void => {
        state.consecutiveFailures += 1;
        state.failureCount += 1;
        sessionLog(sessionId, "rust transform failed; using raw passthrough:", error);
        if (state.consecutiveFailures < RUST_FAILURE_PARK_THRESHOLD || state.parked) return;
        state.parked = true;
        state.parkCount += 1;
        state.passesSincePark = 0;
        state.warningSent = true;
        const warning =
            "Rust Magic Context is unavailable for this session; continuing with an unmodified prompt until the module recovers.";
        sessionLog(sessionId, "rust transform parked after three consecutive failures");
        options.notifyParked?.(sessionId, warning);
    };

    const run = async (
        sessionId: string,
        messages: MessageLike[],
        output: { messages: unknown[] },
        sessionMeta: ReturnType<typeof getOrCreateSessionMeta>,
    ): Promise<void> => {
        const state = ensureState(states, sessionId);
        state.passCount += 1;
        if (state.parked) {
            state.passesSincePark += 1;
            // The fifth live pass is the first retry opportunity after the
            // three-failure park; later retries use the same global cadence.
            if (state.passCount % RUST_PROBE_INTERVAL !== 0) {
                output.messages = messages;
                return;
            }
        }
        const rawMessages = messages;
        try {
            const { directory } = await getSessionDirectory(deps, sessionId);
            const model =
                modelFromMessages(messages) ?? findLastAssistantModelFromOpenCodeDb(sessionId);
            const modelKey = model ? resolveModelKey(model.providerID, model.modelID) : null;
            if (model) deps.liveModelBySession?.set(sessionId, model);
            const usage = loadContextUsage(deps.contextUsageMap, deps.db, sessionId);
            const resolvedContextLimit = model
                ? resolveTrustedContextLimit(model.providerID, model.modelID, {
                      db: deps.db,
                      sessionID: sessionId,
                  })
                : undefined;
            const contextLimit =
                resolvedContextLimit && resolvedContextLimit > 0
                    ? resolvedContextLimit
                    : usage.percentage > 0
                      ? Math.round(usage.inputTokens / (usage.percentage / 100))
                      : 128_000;
            const threshold = resolveExecuteThreshold(
                deps.executeThresholdPercentage ?? 65,
                modelKey ?? undefined,
                65,
                { tokensConfig: deps.executeThresholdTokens, contextLimit },
            );
            const historyBudgetTokens = resolveHistoryBudgetTokens(
                deps.historyBudgetPercentage,
                usage,
                deps.executeThresholdPercentage,
                modelKey ?? undefined,
                deps.executeThresholdTokens,
                resolvedContextLimit,
            );
            const midTurn = isMidTurn(deps, sessionId);
            const passInputs: Record<string, unknown> = {
                now_ms: Date.now(),
                model_key: modelKey,
                usage: passUsage(usage, contextLimit),
                effective_execute_threshold: threshold,
                history_budget_tokens: historyBudgetTokens,
                cache_ttl: sessionMeta.cacheTtl,
                mid_turn: midTurn,
                is_subagent: sessionMeta.isSubagent,
            };
            const resolved = await resolveOrdinalsForModule({
                sessionId,
                messages: cloneForModule(messages),
                generation: state.shadowGeneration,
                memoGeneration: state.idOrdinalMemoGeneration,
                memo: state.idOrdinalMemo,
                memoAnchor: state.ordinalMemoAnchor,
                memoStoredCount: state.ordinalMemoStoredCount,
                memoCanonicalCount: state.ordinalMemoCanonicalCount,
            });
            if (!resolved.ok) throw new Error(`rust ordinal ${resolved.reason}`);
            state.idOrdinalMemoGeneration = resolved.memoGeneration;
            state.ordinalMemoAnchor = resolved.memoAnchor;
            state.ordinalMemoStoredCount = resolved.memoStoredCount;
            state.ordinalMemoCanonicalCount = resolved.memoCanonicalCount;

            const syncPass = {
                db: deps.db,
                sessionId,
                projectPath:
                    deps.memoryConfig?.enabled && directory.length > 0
                        ? resolveProjectIdentity(directory)
                        : deps.projectPath,
                nowMs: Date.now(),
            };
            const projectRoot = options.projectRoot ?? directory;
            await syncModuleState({
                client: { call: callModule },
                state,
                pass: syncPass,
                projectRoot,
                force: !state.initialized,
            });
            const body = buildTransformBody({
                sessionId,
                input: encodeOpenCodeMessagesToCk(resolved.annotatedInput),
                nativeMessages: messages,
                passInputs,
                usage: passUsage(usage, contextLimit),
                modelKey: modelKey ?? null,
                midTurn,
            });
            const pages = buildPagedModuleTransformPayloads(body);
            let response: Record<string, unknown> | undefined;
            for (const page of pages) {
                response = responseValue(
                    await callModule({
                        sessionId,
                        projectRoot,
                        method: "transform",
                        body: page,
                    }),
                );
            }
            if (!response) throw new Error("rust module returned no transform response");
            if (isNeedFullSync(response)) {
                state.initialized = false;
                await syncModuleState({
                    client: { call: callModule },
                    state,
                    pass: syncPass,
                    projectRoot,
                    force: true,
                });
                response = responseValue(
                    await callModule({
                        sessionId,
                        projectRoot,
                        method: "transform",
                        body,
                    }),
                );
            }
            applyNativeMessagesVerbatim(output, response);
            state.initialized = true;
            state.seedPassPending = false;
            state.consecutiveFailures = 0;
            state.parked = false;
            state.passesSincePark = 0;

            const directiveText = directiveTextOf(response);
            if (directiveText) {
                try {
                    casChannel2NudgeState(deps.db, sessionId, "", "pending");
                    await maybeDeliverChannel2(sessionId, {
                        db: deps.db,
                        client: options.hostClient,
                        directiveText,
                    });
                } catch (error) {
                    sessionLog(
                        sessionId,
                        "rust channel2 directive delivery failed (ignored):",
                        error,
                    );
                }
            }
            if (options.moduleClient.getCompartmentsAfter) {
                try {
                    await mirrorModuleCompartments({
                        db: deps.db,
                        sessionId,
                        reader: {
                            getCompartmentsAfter: options.moduleClient.getCompartmentsAfter,
                        } satisfies ModuleCompartmentReader,
                    });
                } catch (error) {
                    sessionLog(sessionId, "rust compartment mirror-back failed (ignored):", error);
                }
            }
        } catch (error) {
            output.messages = rawMessages;
            markFailure(sessionId, state, error);
            if (!state.parked) return;
            // Dogfood behavior intentionally stops at loud raw passthrough while parked;
            // public deployments will need an explicit pressure-triggered re-entry path.
            return;
        }
    };

    return {
        run,
        clearSession(sessionId: string): void {
            states.delete(sessionId);
            options.moduleClient.closeSession?.(sessionId);
        },
        getState(sessionId: string): Readonly<RustSessionState> {
            return {
                ...ensureState(states, sessionId),
                idOrdinalMemo: new Map(ensureState(states, sessionId).idOrdinalMemo),
            };
        },
    };
}

export async function runRustModeTransform(
    transform: ReturnType<typeof createRustModeTransform>,
    sessionId: string,
    messages: MessageLike[],
    output: { messages: unknown[] },
    sessionMeta: ReturnType<typeof getOrCreateSessionMeta>,
): Promise<void> {
    await transform.run(sessionId, messages, output, sessionMeta);
}

export const __rustModeTransformTest = {
    applyNativeMessagesVerbatim,
    buildTransformBody,
    createRustModeTransform,
    directiveTextOf,
};
