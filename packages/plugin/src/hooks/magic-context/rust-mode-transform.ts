import { createHash } from "node:crypto";
import {
    type AuthorityDrainResponse,
    type AuthorityModuleClient,
    type AuthorityStatus,
    checksumAuthoritySeedRows,
    drainAuthority,
    ensureContextStoreUuid,
    prepareAuthority,
    pullMemoryMirrorOnce,
    reconcileAuthorityProject,
} from "../../features/magic-context/context-authority";
import { DEFAULT_PROTECTED_TAGS } from "../../features/magic-context/defaults";
import { resolveProjectIdentity } from "../../features/magic-context/memory/project-identity";
import { getMemoryVerifications } from "../../features/magic-context/memory/storage-memory-verifications";
import type { getOrCreateSessionMeta } from "../../features/magic-context/storage";
import {
    casChannel2NudgeState,
    getChannel2NudgeState,
    getOverflowState,
    isEmergencyRecoveryArmed,
} from "../../features/magic-context/storage-meta-persisted";
import { writeRustTransformDecision } from "../../features/magic-context/transform-decision-log";
import type { ContextUsage } from "../../features/magic-context/types";
import { sessionLog } from "../../shared/logger";
import { resolveCtxReduceAvailability } from "./ctx-reduce-availability";
import {
    resolveExecuteThreshold,
    resolveModelKey,
    resolveTrustedContextLimit,
} from "./event-resolvers";
import { replayLkg, resolveLkgModelKeys } from "./lkg-replay";
import { captureSlot, dropSlot, getSlot, type LkgEntryNote, noteEntry } from "./lkg-slot";
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
import { runRustModePostprocess } from "./transform-postprocess-phase";

export class MemoryAuthorityUnavailableError extends Error {
    readonly code = "MEMORY_AUTHORITY_UNAVAILABLE";

    constructor(detail: string) {
        super(
            `rust memory authority unavailable; route ctx_memory through the Rust module: ${detail}`,
        );
        this.name = "MemoryAuthorityUnavailableError";
    }
}

const RUST_FAILURE_PARK_THRESHOLD = 3;
const RUST_PROBE_INTERVAL = 5;
const RUST_SEND_TIMEOUT_MS = 15_000;

export interface RustModeModuleClient extends ModuleStateSyncClient {
    authorityStatus?(args: {
        context_store_uuid: string;
        project: string;
        /** Bound route root for this authority query. */
        projectRoot?: string;
        domain: "memories" | "notes";
    }): Promise<{ authority: AuthorityStatus | null }>;
    authorityPrepare?(args: Record<string, unknown>): Promise<{ authority: AuthorityStatus }>;
    authoritySeed?(
        args: Record<string, unknown>,
    ): Promise<{ seeded: number; module_row_ids?: number[] }>;
    authorityDrain?(args: Record<string, unknown>): Promise<AuthorityDrainResponse>;
    mirrorPull?(args: {
        domain: "memories" | "notes";
        cursor: number;
        limit: number;
        live_only?: boolean;
        projectRoot?: string;
    }): Promise<{ page: import("../../features/magic-context/context-authority").ChangefeedPage }>;
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
    syntheticTurnCount: number;
    lastObservedUserMessageId: string | null;
    syntheticLoopBreakerLogged: boolean;
    memoryAuthorityProject: string | null;
    memoryAuthorityRoot: string | null;
    memoryAuthorityReady: boolean;
    authorityMemorySyncSkipLogged?: boolean;
}

export interface RustModeTransformOptions {
    moduleClient: RustModeModuleClient;
    hostClient?: unknown;
    projectRoot?: string;
    notifyParked?: (sessionId: string, message: string) => void;
    moduleTimeoutMs?: number;
    memorySyncRequestedSessions?: Set<string>;
    /**
     * Invoked with each project that reaches rust-mode authority preparation, so the
     * host can lazily register per-project services (the smart-note evaluator bridge)
     * for projects other than the plugin's launch directory.
     */
    onProjectPrepared?: (projectPath: string) => void;
    /** Test-only escape hatch for transform-wire tests without an authority transport. */
    allowAuthorityProtocolBypassForTests?: boolean;
}

function cloneForModule<T>(value: T): T {
    return JSON.parse(JSON.stringify(value)) as T;
}

function isRecord(value: unknown): value is Record<string, unknown> {
    return value !== null && typeof value === "object";
}

/**
 * OpenCode retains the original messages array when it serializes a transform result.
 * Mutate that array in place so the module response reaches the wire, while returning
 * the same array for callers that also consume the hook result.
 */
function replaceMessagesInPlace(output: { messages: unknown[] }, next: unknown[]): unknown[] {
    const target = output.messages;
    if (target !== next) target.splice(0, target.length, ...next);
    return target;
}

function messageInfo(value: unknown): Record<string, unknown> {
    if (!isRecord(value)) return {};
    return isRecord(value.info) ? value.info : value;
}

function newestUserMessage(messages: MessageLike[]): MessageLike | undefined {
    for (let index = messages.length - 1; index >= 0; index -= 1) {
        if (messageInfo(messages[index]).role === "user") return messages[index];
    }
    return undefined;
}

function newestAssistantOrUserMessageId(messages: MessageLike[]): string | null {
    for (let index = messages.length - 1; index >= 0; index -= 1) {
        const info = messageInfo(messages[index]);
        if (info.role !== "assistant" && info.role !== "user") continue;
        if (typeof info.id === "string" && info.id.length > 0) return info.id;
    }
    return null;
}

function formatRustPassLog(args: {
    decision: string;
    reason: string;
    servedFrom: string;
    inputCount: number;
    outputCount: number;
    applied: boolean;
    elapsedMs: number;
    moduleElapsedMs: number;
}): string {
    return `rust pass: decision=${args.decision} reason=${args.reason} served_from=${args.servedFrom} in=${args.inputCount} out=${args.outputCount} applied=${args.applied} elapsed=${args.elapsedMs.toFixed(1)} ms module=${args.moduleElapsedMs.toFixed(1)} ms`;
}

function isSyntheticUserMessage(message: MessageLike | undefined): boolean {
    if (!message || messageInfo(message).role !== "user" || !Array.isArray(message.parts)) {
        return false;
    }
    return (
        message.parts.length > 0 &&
        message.parts.every(
            (part) => isRecord(part) && (part.synthetic === true || part.ignored === true),
        )
    );
}

function observeSyntheticTurn(state: RustSessionState, messages: MessageLike[]): boolean {
    const newest = newestUserMessage(messages);
    const info = messageInfo(newest);
    const messageId = typeof info.id === "string" ? info.id : null;
    const synthetic = isSyntheticUserMessage(newest);
    const isNewMessage = messageId === null || messageId !== state.lastObservedUserMessageId;

    if (!synthetic) {
        state.syntheticTurnCount = 0;
        state.syntheticLoopBreakerLogged = false;
    } else if (isNewMessage) {
        state.syntheticTurnCount += 1;
    }
    state.lastObservedUserMessageId = messageId;
    return synthetic;
}

function assertNativeBoundary(output: unknown[], sessionId: string, boundaryId: string): void {
    const first = output.find((message) => messageInfo(message).role !== "system");
    const info = messageInfo(first);
    const parts = isRecord(first) && Array.isArray(first.parts) ? first.parts : [];
    const synthetic =
        parts.length > 0 && parts.every((part) => isRecord(part) && part.synthetic === true);
    if (info.role === "user" && info.sessionID === sessionId && synthetic) return;
    throw new Error(
        `rust transform wire invariant failed: boundary=${boundaryId} expected a synthetic m0 user message scoped to session ${sessionId}`,
    );
}

function responseValue(response: unknown): Record<string, unknown> {
    if (isRecord(response) && isRecord(response.result)) return response.result;
    if (isRecord(response)) return response;
    throw new Error("module transform returned a non-object response");
}

function noteDeliveryPassIds(response: Record<string, unknown>): string[] {
    if (!Array.isArray(response.note_deliveries)) return [];
    return [
        ...new Set(
            response.note_deliveries.flatMap((delivery) => {
                if (!isRecord(delivery)) return [];
                const passId = delivery.transform_pass_id;
                return typeof passId === "string" && passId.length > 0 ? [passId] : [];
            }),
        ),
    ];
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
            syntheticTurnCount: 0,
            lastObservedUserMessageId: null,
            syntheticLoopBreakerLogged: false,
            memoryAuthorityProject: null,
            memoryAuthorityRoot: null,
            memoryAuthorityReady: false,
            authorityMemorySyncSkipLogged: false,
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

function readUpgradeState(db: TransformDeps["db"], sessionId: string): string {
    const row = db
        .prepare("SELECT COUNT(*) AS count FROM compartments WHERE session_id = ? AND legacy = 1")
        .get(sessionId) as { count?: number } | undefined;
    return (row?.count ?? 0) > 0 ? "legacy" : "ready";
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

function canonicalizeForChecksum(value: unknown): unknown {
    if (Array.isArray(value)) return value.map(canonicalizeForChecksum);
    if (!isRecord(value)) return value;
    return Object.fromEntries(
        Object.keys(value)
            .sort()
            .map((key) => [key, canonicalizeForChecksum(value[key])]),
    );
}

function checksumSeedRows(rows: readonly Record<string, unknown>[]): string {
    return createHash("sha256")
        .update(JSON.stringify(rows.map(canonicalizeForChecksum)))
        .digest("hex");
}

function authoritySeedRows(
    db: TransformDeps["db"],
    projectPath: string,
    domain: "memories" | "notes",
): Record<string, unknown>[] {
    const snapshots =
        domain === "memories"
            ? db
                  .prepare("SELECT * FROM memories WHERE project_path = ? ORDER BY id ASC")
                  .all(projectPath)
            : db
                  .prepare(
                      `SELECT n.*
                         FROM notes n
                        WHERE n.project_path = ?
                           OR (n.project_path IS NULL AND EXISTS (
                               SELECT 1 FROM session_projects sp
                                WHERE sp.session_id = n.session_id AND sp.project_path = ?
                           ))
                        ORDER BY n.id ASC`,
                  )
                  .all(projectPath, projectPath);
    const memoryRows = snapshots.filter(isRecord);
    const mappings =
        domain === "memories"
            ? getMemoryVerifications(
                  db,
                  memoryRows.map((row) => Number(row.id)),
              )
            : new Map<number, { files: string[]; hasSentinel: boolean }>();
    return memoryRows.map((snapshot) => {
        const id = Number(snapshot.id);
        const mapping = mappings.get(id);
        const seededSnapshot =
            domain === "memories" && mapping
                ? { ...snapshot, mapping: mapping.hasSentinel ? null : mapping.files }
                : domain === "notes" && snapshot.project_path == null
                  ? { ...snapshot, project_path: projectPath }
                  : snapshot;
        return { source_row_id: snapshot.id, snapshot: seededSnapshot };
    });
}

async function prepareRustMemoryAuthority(args: {
    db: TransformDeps["db"];
    module: RustModeModuleClient;
    projectPath: string;
    projectRoot: string;
    state: RustSessionState;
    allowProtocolBypassForTests?: boolean;
    /** Fires after authority is ready so hosts can register per-project services. */
    onProjectPrepared?: (projectPath: string) => void;
}): Promise<void> {
    const { db, module, projectPath, projectRoot, state } = args;
    if (
        state.memoryAuthorityProject === projectPath &&
        state.memoryAuthorityRoot === projectRoot &&
        state.memoryAuthorityReady
    ) {
        return;
    }
    state.memoryAuthorityProject = projectPath;
    state.memoryAuthorityRoot = projectRoot;
    state.memoryAuthorityReady = false;
    if (!module.authorityStatus || !module.authorityPrepare || !module.authoritySeed) {
        if (args.allowProtocolBypassForTests === true) {
            state.memoryAuthorityReady = true;
            return;
        }
        throw new MemoryAuthorityUnavailableError(
            "the module does not expose authority.status, authority.prepare, and authority.seed",
        );
    }

    // Call through the module object on every invocation: these may be real class
    // methods whose implementations depend on their instance, so detaching them into
    // locals would sever `this` and only fail at runtime (test fakes are object
    // literals and cannot catch the difference).
    const authorityModule: AuthorityModuleClient = {
        authorityStatus: (request) => module.authorityStatus!({ ...request, projectRoot }),
        authorityPrepare: (request) => module.authorityPrepare!({ ...request, projectRoot }),
        authoritySeed: (request) => module.authoritySeed!({ ...request, projectRoot }),
        authorityDrain: module.authorityDrain
            ? (request) => module.authorityDrain!({ ...request, projectRoot })
            : undefined,
        mirrorPull: module.mirrorPull
            ? (request) => module.mirrorPull!({ ...request, projectRoot })
            : undefined,
    };
    const contextStoreUuid = ensureContextStoreUuid(db);
    const domains = ["memories", "notes"] as const;
    const statuses = new Map<
        (typeof domains)[number],
        Awaited<ReturnType<NonNullable<RustModeModuleClient["authorityStatus"]>>>["authority"]
    >();
    for (const domain of domains) {
        const current = await authorityModule.authorityStatus({
            context_store_uuid: contextStoreUuid,
            project: projectPath,
            domain,
        });
        statuses.set(domain, current.authority);
    }

    let resumedDrain = false;
    for (const domain of domains) {
        const current = statuses.get(domain);
        if (current?.state !== "DRAINING") continue;
        resumedDrain = true;
        let drained: Awaited<ReturnType<typeof drainAuthority>> | undefined;
        for (let attempt = 0; attempt < 2; attempt += 1) {
            drained = await drainAuthority({
                db,
                projectPath,
                domain,
                module: authorityModule,
                checksum: () =>
                    checksumSeedRows(
                        db
                            .prepare(
                                `SELECT * FROM ${domain === "memories" ? "memories" : "notes"} WHERE project_path = ? ORDER BY id ASC`,
                            )
                            .all(projectPath)
                            .filter(isRecord),
                    ),
            });
            if (!("code" in drained)) break;
        }
        if (!drained) {
            throw new MemoryAuthorityUnavailableError("authority drain did not return a result");
        }
        if ("code" in drained) {
            throw new MemoryAuthorityUnavailableError(
                `${drained.code}; the next scheduled transform will resume the drain`,
            );
        }
        statuses.set(domain, null);
    }

    // Do not return before finishing authority restore: if some domains are still
    // DRAINING and others MODULE, reinstall the on-disk authority_managed marker and
    // re-apply write fences on remaining MODULE domains before any tools run.
    if (!resumedDrain) {
        for (const domain of domains) {
            const current = statuses.get(domain);
            if (current?.state !== "PREPARING") continue;
            await authorityModule.authorityPrepare({
                method: "authority.prepare",
                phase: "abort",
                context_store_uuid: contextStoreUuid,
                project: projectPath,
                domain,
                generation: current.generation,
            });
            statuses.set(domain, null);
        }
        const preparing = domains.filter((domain) => statuses.get(domain)?.state !== "MODULE");
        for (const domain of preparing) {
            const stateName = statuses.get(domain)?.state;
            if (stateName && stateName !== "TS") {
                throw new Error(`${domain} authority cannot prepare from ${stateName}`);
            }
        }
        if (preparing.length > 0) {
            await prepareAuthority({
                db,
                projectPath,
                domains: preparing,
                module: authorityModule,
                seedPages: async (domain) => authoritySeedRows(db, projectPath, domain),
                checksum: (_domain, rows) => checksumAuthoritySeedRows(rows),
            });
        }
    }

    await reconcileAuthorityProject({ db, projectPath, module: authorityModule });
    state.memoryAuthorityReady = true;
    args.onProjectPrepared?.(projectPath);
}

/** Single response-field seam for the parallel module encode-back contract. */
export function applyNativeMessagesVerbatim(
    output: { messages: unknown[] },
    response: Record<string, unknown>,
): unknown[] {
    const nativeMessages = response.native_messages;
    if (typeof nativeMessages === "string") {
        const parsed = JSON.parse(nativeMessages) as unknown;
        if (!Array.isArray(parsed))
            throw new Error("rust transform native_messages string was not an array");
        return replaceMessagesInPlace(output, parsed);
    }
    if (!Array.isArray(nativeMessages)) {
        throw new Error("rust transform response omitted native_messages");
    }
    // The module owns healing, ordering, and codec fidelity. Do not clone,
    // normalize, or otherwise inspect the returned native message array.
    return replaceMessagesInPlace(output, nativeMessages);
}

function buildTransformBody(args: {
    sessionId: string;
    input: unknown[];
    nativeMessages: unknown[];
    passInputs: Record<string, unknown>;
    usage: Record<string, number>;
    modelKey: string | null;
    providerId: string | null;
    systemPromptHash: string;
    upgradeState: string;
    midTurn: boolean;
    prevResponseCompletedAtMs?: number;
    requestObservedAtMs?: number;
    channel2NudgeState: string;
    emergencyRecoveryArmed: boolean;
    declaredTrim?: unknown;
}): Record<string, unknown> {
    return {
        method: "transform",
        kind: "transform",
        v: 2,
        serializer_profile: "opencode-aisdk",
        serve_native: true,
        session_id: args.sessionId,
        // Model/provider and system-prompt changes are provider-cache eviction signals;
        // send the same identity inputs used by the TypeScript materializer instead of
        // leaving the native identity blank.
        render_config: [
            args.providerId ? `provider:${args.providerId}` : "",
            args.modelKey ? `model:${args.modelKey}` : "",
            args.systemPromptHash ? `system:${args.systemPromptHash}` : "",
        ]
            .filter(Boolean)
            .join("|"),
        system_prompt_hash: args.systemPromptHash,
        upgrade_state: args.upgradeState,
        is_subagent: args.passInputs.is_subagent === true,
        protected_tags: args.passInputs.protected_tags ?? DEFAULT_PROTECTED_TAGS,
        messages: args.input,
        native_messages: args.nativeMessages,
        usage: args.usage,
        provider_error: args.passInputs.provider_error,
        mid_turn: args.midTurn,
        prev_response_completed_at_ms: args.prevResponseCompletedAtMs,
        request_observed_at_ms: args.requestObservedAtMs,
        channel2_nudge_state: args.channel2NudgeState,
        emergency_recovery_armed: args.emergencyRecoveryArmed,
        model_key: args.modelKey,
        provider_id: args.providerId,
        tool_present: args.passInputs.tool_present === true,
        effective_execute_threshold: args.passInputs.effective_execute_threshold,
        history_budget_tokens: args.passInputs.history_budget_tokens,
        clear_reasoning_age: args.passInputs.clear_reasoning_age,
        caveman_enabled: args.passInputs.caveman_enabled === true,
        caveman_min_chars: args.passInputs.caveman_min_chars ?? 500,
        cache_ttl: args.passInputs.cache_ttl,
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
        sessionLog(sessionId, "rust transform failed; attempting LKG replay:", error);
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

    const replayLastGood = (
        sessionId: string,
        currentMessages: MessageLike[],
        output: { messages: unknown[] },
    ): boolean => {
        const slot = getSlot(sessionId);
        if (!slot) {
            sessionLog(sessionId, "lkg_miss");
            return false;
        }
        if (isEmergencyRecoveryArmed(sessionId)) {
            sessionLog(sessionId, "lkg_emergency_armed");
            return false;
        }
        try {
            if (getOverflowState(deps.db, sessionId).needsEmergencyRecovery) {
                sessionLog(sessionId, "lkg_emergency_armed");
                return false;
            }
        } catch {
            return false;
        }
        let entry: LkgEntryNote | null = null;
        try {
            entry = noteEntry(sessionId, currentMessages);
        } catch (error) {
            sessionLog(sessionId, "rust LKG entry snapshot failed:", error);
            return false;
        }
        if (!entry) {
            dropSlot(sessionId, "lkg_invalidated_reshape");
            sessionLog(sessionId, "lkg_invalidated_reshape");
            return false;
        }
        const keys = resolveLkgModelKeys(currentMessages);
        const replay = replayLkg({
            sessionId,
            messages: currentMessages,
            modelKey: keys.modelKey,
            providerKey: keys.providerKey,
            entry,
            skipSeamValidation: true,
        });
        if (!replay.ok) {
            sessionLog(sessionId, replay.reason);
            return false;
        }
        replaceMessagesInPlace(output, replay.messages);
        sessionLog(sessionId, "lkg_replay_served");
        return true;
    };

    const captureRustResponse = (
        sessionId: string,
        input: MessageLike[],
        response: Record<string, unknown>,
    ): void => {
        const ids = input.map((message) => message.info.id);
        if (
            ids.some((id) => typeof id !== "string") ||
            new Set(ids).size !== ids.length ||
            ids.length === 0
        )
            return;
        const native = response.native_messages;
        const jsonPrefix = typeof native === "string" ? native : JSON.stringify(native);
        if (typeof jsonPrefix !== "string") return;
        const keys = resolveLkgModelKeys(input);
        captureSlot(sessionId, {
            jsonPrefix,
            inputIdSeq: ids as string[],
            lastInputMessageId: ids[ids.length - 1] as string,
            modelKey: keys.modelKey,
            providerKey: keys.providerKey,
            capturedAt: Date.now(),
        });
    };

    const run = async (
        sessionId: string,
        messages: MessageLike[],
        output: { messages: unknown[] },
        sessionMeta: ReturnType<typeof getOrCreateSessionMeta>,
    ): Promise<void> => {
        const passStartedAt = performance.now();
        const state = ensureState(states, sessionId);
        state.passCount += 1;
        const syntheticTurn = observeSyntheticTurn(state, messages);
        const syntheticLoopBlocked = syntheticTurn && state.syntheticTurnCount >= 3;
        if (syntheticLoopBlocked && !state.syntheticLoopBreakerLogged) {
            state.syntheticLoopBreakerLogged = true;
            sessionLog(
                sessionId,
                "RUST LOOP BREAKER: suppressing host directives after three consecutive synthetic turns until a real user message arrives",
            );
        }
        const inputCount = messages.length;
        let requestInputTokens = 0;
        const newestMessageId = newestAssistantOrUserMessageId(messages);
        let decision = "error";
        let materializeReason = "none";
        let servedFrom = "none";
        let moduleElapsedMs = 0;
        let appliedAt: number | undefined;
        const finishPass = (applied: boolean): void => {
            const elapsedAt = applied && appliedAt !== undefined ? appliedAt : performance.now();
            const elapsedMs = Math.max(0, elapsedAt - passStartedAt);
            sessionLog(
                sessionId,
                formatRustPassLog({
                    decision,
                    reason: materializeReason,
                    servedFrom,
                    inputCount,
                    outputCount: output.messages.length,
                    applied,
                    elapsedMs,
                    moduleElapsedMs,
                }),
            );
        };
        const captureResponseTelemetry = (response: Record<string, unknown>): void => {
            decision =
                typeof response.decision === "string"
                    ? response.decision
                    : typeof response.action === "string"
                      ? response.action
                      : typeof response.status === "string"
                        ? response.status
                        : "unknown";
            servedFrom =
                typeof response.served_from === "string" ? response.served_from : "unknown";
            materializeReason =
                typeof response.materialize_reason === "string" &&
                response.materialize_reason.length > 0
                    ? response.materialize_reason
                    : "none";
            const timings = isRecord(response.timings) ? response.timings : undefined;
            const total = timings?.total;
            moduleElapsedMs = typeof total === "number" && Number.isFinite(total) ? total : 0;
        };
        if (state.parked) {
            state.passesSincePark += 1;
            // The fifth live pass is the first retry opportunity after the
            // three-failure park; later retries use the same global cadence.
            if (state.passCount % RUST_PROBE_INTERVAL !== 0) {
                decision = "parked";
                const replayed = replayLastGood(sessionId, messages, output);
                if (replayed) {
                    servedFrom = "lkg";
                } else {
                    servedFrom = "raw";
                    replaceMessagesInPlace(output, messages);
                }
                finishPass(false);
                return;
            }
        }
        const rawMessages = messages.slice();
        const reduceAvailability = resolveCtxReduceAvailability(sessionId);
        // A provisional fail-open verdict must not activate provider-visible bytes. The
        // first persisted user message freezes the verdict for all later transform passes.
        const toolPresent = reduceAvailability.frozen && reduceAvailability.callable;
        try {
            const { directory } = await getSessionDirectory(deps, sessionId);
            const model =
                modelFromMessages(messages) ?? findLastAssistantModelFromOpenCodeDb(sessionId);
            const modelKey = model ? resolveModelKey(model.providerID, model.modelID) : null;
            if (model) deps.liveModelBySession?.set(sessionId, model);
            const usage = loadContextUsage(deps.contextUsageMap, deps.db, sessionId);
            requestInputTokens = Math.max(0, Math.floor(usage.inputTokens));
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
            const requestObservedAtMs = Date.now();
            const overflowState = getOverflowState(deps.db, sessionId, modelKey);
            const passInputs: Record<string, unknown> = {
                now_ms: requestObservedAtMs,
                model_key: modelKey,
                provider_id: model?.providerID ?? null,
                usage: passUsage(usage, contextLimit),
                effective_execute_threshold: threshold,
                history_budget_tokens: historyBudgetTokens,
                clear_reasoning_age: deps.clearReasoningAge,
                caveman_enabled:
                    !sessionMeta.isSubagent && deps.cavemanTextCompression?.enabled === true,
                caveman_min_chars: deps.cavemanTextCompression?.minChars ?? 500,
                cache_ttl: sessionMeta.cacheTtl,
                mid_turn: midTurn,
                is_subagent: sessionMeta.isSubagent,
                system_prompt_hash: sessionMeta.systemPromptHash ?? "",
                upgrade_state: readUpgradeState(deps.db, sessionId),
                tool_present: toolPresent,
                protected_tags: deps.protectedTags ?? DEFAULT_PROTECTED_TAGS,
                temporal_awareness: deps.experimentalTemporalAwareness === true,
                channel2_nudge_state: getChannel2NudgeState(deps.db, sessionId),
                emergency_recovery_armed:
                    overflowState.needsEmergencyRecovery || isEmergencyRecoveryArmed(sessionId),
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
            if (!resolved.ok) {
                throw new Error(
                    `rust ordinal ${resolved.reason}: messageId=${resolved.messageId ?? "unknown"} ` +
                        `index=${resolved.messageIndex ?? "unknown"} role=${resolved.messageRole ?? "unknown"}`,
                );
            }
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
            const memoryProjectPath =
                deps.memoryConfig?.enabled && directory.length > 0
                    ? resolveProjectIdentity(directory)
                    : deps.projectPath;
            await prepareRustMemoryAuthority({
                db: deps.db,
                module: options.moduleClient,
                projectPath: memoryProjectPath ?? projectRoot,
                projectRoot,
                state,
                allowProtocolBypassForTests: options.allowAuthorityProtocolBypassForTests,
                onProjectPrepared: options.onProjectPrepared,
            });
            const authoritySeqAdoption = { used: false };
            if (options.memorySyncRequestedSessions?.delete(sessionId)) {
                // A memory tool call can complete after the prior authority pass has
                // acknowledged its watermarks. Rewind only memory watermarks so the
                // next pass ships the mutation delta without reseeding compartments.
                const watermarks = state.lastAckedWatermarks;
                if (watermarks) {
                    state.lastAckedWatermarks = {
                        ...watermarks,
                        memory_id: 0,
                        memory_mutation_id: 0,
                    };
                }
            }
            await syncModuleState({
                client: { call: callModule },
                state,
                pass: syncPass,
                projectRoot,
                force: !state.initialized,
                options: {
                    authority: true,
                    authorityState: state.memoryAuthorityReady ? "MODULE" : undefined,
                    authoritySeqAdoption,
                },
            });
            const body = buildTransformBody({
                sessionId,
                input: encodeOpenCodeMessagesToCk(resolved.annotatedInput),
                nativeMessages: messages,
                passInputs,
                usage: passUsage(usage, contextLimit),
                modelKey: modelKey ?? null,
                providerId: model?.providerID ?? null,
                systemPromptHash: sessionMeta.systemPromptHash ?? "",
                upgradeState: String(passInputs.upgrade_state ?? ""),
                midTurn,
                prevResponseCompletedAtMs:
                    sessionMeta.lastResponseTime > 0 ? sessionMeta.lastResponseTime : undefined,
                requestObservedAtMs,
                channel2NudgeState: String(passInputs.channel2_nudge_state ?? ""),
                emergencyRecoveryArmed: passInputs.emergency_recovery_armed === true,
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
            captureResponseTelemetry(response);
            if (isNeedFullSync(response)) {
                state.initialized = false;
                await syncModuleState({
                    client: { call: callModule },
                    state,
                    pass: syncPass,
                    projectRoot,
                    force: true,
                    options: {
                        authority: true,
                        authorityState: state.memoryAuthorityReady ? "MODULE" : undefined,
                        authoritySeqAdoption,
                    },
                });
                response = undefined;
                for (const page of buildPagedModuleTransformPayloads(body)) {
                    response = responseValue(
                        await callModule({
                            sessionId,
                            projectRoot,
                            method: "transform",
                            body: page,
                        }),
                    );
                }
                if (!response) throw new Error("rust module returned no retry transform response");
                captureResponseTelemetry(response);
            }
            if (newestMessageId) {
                writeRustTransformDecision({
                    db: deps.db,
                    sessionId,
                    messageId: newestMessageId,
                    decision,
                    materializeReason: materializeReason === "none" ? null : materializeReason,
                    inputTokens: requestInputTokens,
                });
            }
            const deliveryPassIds = noteDeliveryPassIds(response);
            const sendNoteDeliveryDisposition = async (
                method: "transform.ack" | "transform.nack",
            ) => {
                for (const transformPassId of deliveryPassIds) {
                    await callModule({
                        sessionId,
                        projectRoot,
                        method,
                        body: {
                            method,
                            v: 1,
                            session_id: sessionId,
                            transform_pass_id: transformPassId,
                        },
                    });
                }
            };
            let appliedMessages: unknown[];
            try {
                appliedMessages = applyNativeMessagesVerbatim(output, response);
                runRustModePostprocess({
                    db: deps.db,
                    sessionId,
                    messages: appliedMessages as MessageLike[],
                    projectPath: memoryProjectPath,
                    fullFeatureMode: !sessionMeta.isSubagent,
                });
                const boundaryId = response.boundary_id;
                if (typeof boundaryId === "string" && boundaryId.length > 0) {
                    assertNativeBoundary(appliedMessages, sessionId, boundaryId);
                }
                appliedAt = performance.now();
            } catch (error) {
                try {
                    await sendNoteDeliveryDisposition("transform.nack");
                } catch (nackError) {
                    sessionLog(sessionId, "rust note delivery nack failed (ignored):", nackError);
                }
                throw error;
            }
            if (deliveryPassIds.length > 0) {
                try {
                    await sendNoteDeliveryDisposition("transform.ack");
                } catch (ackError) {
                    // Leave the delivery unacknowledged when the acknowledgement transport
                    // fails; the module will re-serve those bytes on a later natural bust.
                    sessionLog(sessionId, "rust note delivery ack failed (will retry):", ackError);
                }
            }
            captureRustResponse(sessionId, rawMessages, response);
            state.initialized = true;
            state.seedPassPending = false;
            state.consecutiveFailures = 0;
            state.parked = false;
            state.passesSincePark = 0;

            const directiveText = directiveTextOf(response);
            if (syntheticTurn) {
                // A pending lease must not escape the breaker through the terminal
                // event handler while synthetic turns are cascading.
                try {
                    casChannel2NudgeState(deps.db, sessionId, "pending", "");
                    deps.channel2DirectiveTextBySession?.delete(sessionId);
                } catch {
                    // The delivery lease remains authoritative if another sender owns it.
                }
            } else if (directiveText) {
                // The module only recommends Channel 2 here. Delivery must wait for the
                // terminal message.updated boundary, where the host's shared claim/CAS
                // path revalidates the lease and coalesces the synthetic user turn.
                try {
                    casChannel2NudgeState(deps.db, sessionId, "", "pending");
                    deps.channel2DirectiveTextBySession?.set(sessionId, directiveText);
                } catch (error) {
                    sessionLog(
                        sessionId,
                        "rust channel2 pending-intent CAS failed (ignored):",
                        error,
                    );
                }
            }
            if (options.moduleClient.mirrorPull) {
                try {
                    await pullMemoryMirrorOnce({
                        db: deps.db,
                        module: options.moduleClient as AuthorityModuleClient,
                    });
                } catch (error) {
                    sessionLog(sessionId, "rust memory mirror-back failed (ignored):", error);
                }
            }
            if (options.moduleClient.getCompartmentsAfter) {
                try {
                    await mirrorModuleCompartments({
                        db: deps.db,
                        sessionId,
                        reader: {
                            getCompartmentsAfter: (mirroredSessionId, afterSequence) =>
                                options.moduleClient.getCompartmentsAfter!(
                                    mirroredSessionId,
                                    afterSequence,
                                ),
                        } satisfies ModuleCompartmentReader,
                    });
                } catch (error) {
                    sessionLog(sessionId, "rust compartment mirror-back failed (ignored):", error);
                }
            }
            finishPass(true);
        } catch (error) {
            if (
                error instanceof Error &&
                error.message.startsWith("rust transform wire invariant failed")
            ) {
                sessionLog(
                    sessionId,
                    "rust transform wire invariant failed; LKG replay required",
                    error,
                );
            }
            // Restore the caller-owned raw array before attempting LKG replay. The
            // invariant is checked after in-place application, so a failed check may
            // have already replaced its contents with an untrusted module response.
            replaceMessagesInPlace(output, rawMessages);
            const replayed = replayLastGood(sessionId, rawMessages, output);
            if (!replayed) replaceMessagesInPlace(output, rawMessages);
            markFailure(sessionId, state, error);
            finishPass(false);
            return;
        }
    };

    return {
        run,
        clearSession(sessionId: string): void {
            dropSlot(sessionId, "session-deleted");
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
    formatRustPassLog,
    createRustModeTransform,
    directiveTextOf,
    prepareRustMemoryAuthority,
};
