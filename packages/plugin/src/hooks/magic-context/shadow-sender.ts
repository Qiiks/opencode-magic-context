import { createHash, createHmac } from "node:crypto";
import { join } from "node:path";
import {
    AdmissionClass,
    type BindIdentity,
    isConsumerReconnectTransient,
    Priority,
    type RouteHandle,
    type RouteTarget,
    SocketClosedError,
    SocketTimeoutError,
    StaleRouteHandleError,
    SubcClient,
} from "@cortexkit/subc-client";
import { getCompartmentsByEndMessageId } from "../../features/magic-context/compartment-storage";
import type {
    AuthorityStatus,
    ChangefeedPage,
} from "../../features/magic-context/context-authority";
import type { ContextDatabase } from "../../features/magic-context/storage";
import {
    getAutoSearchHintDecisions,
    getPersistedCompactionMarkerState,
} from "../../features/magic-context/storage-meta-persisted";
import { getDataDir } from "../../shared/data-path";
import { getHarness } from "../../shared/harness";
import { sessionLog } from "../../shared/logger";
import { isRecord } from "../../shared/record-type-guard";
import {
    buildModuleStateSyncPayload,
    buildPagedModuleStateSyncPayloads,
    canonicalOrdinalForMessageId,
    type ModuleStateSyncPayload,
} from "./module-state-sync";
import {
    buildPagedModuleTransformPayloads,
    moduleWireBodyBytes,
    resolveOrdinalsForModule,
    toFlatModuleWireBody,
} from "./module-wire";
import {
    readRawSessionMessageOrdinalById,
    readRawSessionMessagePartsById,
} from "./read-session-chunk";
import {
    isRawCompactionSummaryInfo,
    type RawMessageOrdinalAnchor,
    type RawMessageParts,
} from "./read-session-raw";
import type { MessageLike, TagNormalizationTarget } from "./tag-messages";

const DEFAULT_MODULE_ID = "magic-context";
function getDefaultConnectionFile(): string {
    return join(getDataDir(), "cortexkit", "run", "subc-connection.json");
}
const CONNECT_BACKOFF_INITIAL_MS = 1_000;
const CONNECT_BACKOFF_MAX_MS = 30_000;
const HANDSHAKE_TIMEOUT_MS = 2_000;
const SHADOW_SEND_TIMEOUT_MS = 15_000;
const SHADOW_QUEUE_MAX_DEPTH = 2;
const RESEED_COOLDOWN_MS = 30 * 60 * 1_000;
const RESEED_ATTEMPT_CAP = 5;
const SHADOW_SEED_BUDGET_MS = 30_000;
const SHADOW_SEED_BATCH_MAX_BYTES = 512 * 1024;
const SHADOW_TRANSFORM_PAGE_MAX_BYTES = SHADOW_SEED_BATCH_MAX_BYTES;
const SHADOW_RESET_REASON_RING_SIZE = 8;
const SHADOW_SEND_FAILURE_PARK_THRESHOLD = 3;
const MAX_FACADE_FRAME_BYTES = 1024 * 1024;

const declaredTrimBySession = new Map<string, { markerKey: string; trim: ShadowDeclaredTrim }>();

export type ShadowDecisionClass = "defer" | "soft" | "hard";

export interface ShadowPassInputs {
    now_ms: number;
    model_key: string | null;
    usage: {
        input_tokens: number;
        limit: number;
    };
    effective_execute_threshold: number;
    history_budget_tokens: number;
    cache_ttl: string;
    provider_error?: string;
    /** True when the newest assistant message is still streaming. */
    mid_turn: boolean;
    /** Session-mode hint consumed by the module's session-mode machinery. */
    is_subagent?: boolean;
}

export interface ShadowTransformDecision {
    class: ShadowDecisionClass;
    marker_state: {
        marker_message_id?: string;
        advanced_this_pass: boolean;
    };
    materialize_reason?: string | null;
    emergency?: boolean;
}

export interface ShadowDeclaredTrim {
    flat_boundary_id: string;
    boundary_bare_message_id: string;
    boundary_absolute_ordinal: number;
    next_absolute_ordinal: number;
}

export interface ShadowTransformPass {
    sessionId: string;
    isSubagent: boolean;
    db: ContextDatabase;
    projectRoot: string;
    projectPath?: string;
    inputMessages: MessageLike[];
    outputMessages: MessageLike[];
    normalizationTargets: readonly TagNormalizationTarget[];
    passInputs: ShadowPassInputs;
    tsDecision: ShadowTransformDecision;
    declaredTrimBefore: ShadowDeclaredTrim | null;
}

export interface ShadowTransport {
    call(args: {
        sessionId: string;
        projectRoot: string;
        method:
            | "shadow_reset"
            | "state_sync"
            | "shadow_transform"
            | "transform"
            | "session.status"
            | "session.flush"
            | "session.recomp"
            | "session.wrapup"
            | "todo_state.set"
            | "ctx_note"
            | "ctx_memory"
            | "note.evaluate"
            | "transform.ack"
            | "transform.nack";
        body: unknown;
        signal?: AbortSignal;
    }): Promise<unknown>;
    closeSession?(sessionId: string): void;
}

export interface ShadowSender {
    enqueue(pass: ShadowTransformPass): void;
    resetSession(sessionId: string, reason: string): void;
    clearSession(sessionId: string): void;
    getStats(sessionId: string): Readonly<ShadowSenderCounters>;
    getQueueDepth(sessionId: string): number;
}

interface ShadowSenderCounters {
    enqueued: number;
    dropped_oldest: number;
    send_failures: number;
    send_timeouts: number;
    connection_skips: number;
    ordinal_unresolved: number;
    ordinal_mismatch: number;
    state_sync_rejects: number;
    generation_rejects: number;
    resets_sent: number;
    transforms_sent: number;
    seed_budget_exceeded: number;
    parked: number;
}

interface SessionQueueState {
    queue: ShadowWorkItem[];
    running: boolean;
    initialized: boolean;
    shadowGeneration: number;
    lastAckedSeq: number;
    lastAckedWatermarks: ShadowWatermarks | null;
    idOrdinalMemoGeneration: number;
    idOrdinalMemo: Map<string, number>;
    idOrdinalMemoAnchor: RawMessageOrdinalAnchor | null;
    idOrdinalMemoStoredCount: number | null;
    idOrdinalMemoCanonicalCount: number;
    requireResetReason: string | null;
    blockedUntilReset: boolean;
    seedPassPending: boolean;
    skipped: boolean;
    reseedAttempts: number;
    lastReseedAttemptMs: number | null;
    reseedAwaitingSuccess: boolean;
    seedStartedAtMs: number | null;
    seedBudgetSpentMs: number;
    resetReasons: string[];
    sendFailureClass: string | null;
    consecutiveSendFailures: number;
    parkedReason: "send_failure" | "reset_repeat" | null;
    parked: boolean;
    counters: ShadowSenderCounters;
}

type ShadowWorkItem =
    | { kind: "pass"; pass: ShadowTransformPass }
    | {
          kind: "reset";
          sessionId: string;
          reason: string;
          projectRoot?: string;
          db?: ContextDatabase;
          projectPath?: string;
      };

interface PreparedShadowPass extends ShadowTransformPass {
    annotatedInput: unknown[];
    shadowTsOutput?: unknown[];
    shadowNormalizations?: ShadowNormalizationRecord[];
    declaredTrim: ShadowDeclaredTrim | null;
}

interface ShadowWatermarks {
    compartment_sequence: number;
    memory_id: number;
    m0_mutation_id: number;
    memory_mutation_id: number;
    last_todo_state_hash: string;
}

interface ShadowWorkspacePayload {
    fingerprint: string;
    members: Array<{ project_path: string; share_categories: string[] }>;
}

interface ShadowWorkspaceContext {
    workspace: ShadowWorkspacePayload | null;
    expandedIdentities: string[];
    ownIdentities: string[];
    shareCategories: string[] | null;
}

interface ShadowStateSyncPayload {
    method: "state_sync";
    params: {
        shadow_generation: number;
        expected_shadow_seq: number;
        seed_id?: string;
        seed_generation?: number;
        seed_batch_index?: number;
        seed_batch_total?: number;
        seed_complete?: boolean;
        seed_boundary_id?: string | null;
        compartments: unknown[];
        memories: unknown[];
        memory_mutations: unknown[];
        user_profile: string[];
        workspace?: ShadowWorkspacePayload | null;
        last_todo_state?: string;
        acked_watermarks?: ShadowWatermarks;
    };
    watermarks: ShadowWatermarks;
    wireBatches?: ShadowStateSyncPayload[];
}

type ShadowSeedItem =
    | { kind: "compartment"; value: unknown }
    | { kind: "memory"; value: unknown }
    | { kind: "memory_mutation"; value: unknown }
    | { kind: "user_profile"; value: string };

function flatWireBodyBytes(payload: ShadowStateSyncPayload): number {
    return moduleWireBodyBytes(payload);
}

function buildPagedSeedPayloads(args: {
    shadowGeneration: number;
    expectedShadowSeq: number;
    seedId: string;
    seedBoundaryId: string | null;
    compartments: unknown[];
    memories: unknown[];
    memoryMutations: unknown[];
    userProfile: string[];
    workspace: ShadowWorkspacePayload | null;
    lastTodoState: string;
    watermarks: ShadowWatermarks;
}): ShadowStateSyncPayload[] {
    return buildPagedModuleStateSyncPayloads(args) as ShadowStateSyncPayload[];
}

function emptyCounters(): ShadowSenderCounters {
    return {
        enqueued: 0,
        dropped_oldest: 0,
        send_failures: 0,
        send_timeouts: 0,
        connection_skips: 0,
        ordinal_unresolved: 0,
        ordinal_mismatch: 0,
        state_sync_rejects: 0,
        generation_rejects: 0,
        resets_sent: 0,
        transforms_sent: 0,
        seed_budget_exceeded: 0,
        parked: 0,
    };
}

function createSessionQueueState(): SessionQueueState {
    return {
        queue: [],
        running: false,
        initialized: false,
        shadowGeneration: 0,
        lastAckedSeq: 0,
        lastAckedWatermarks: null,
        idOrdinalMemoGeneration: 0,
        idOrdinalMemo: new Map(),
        idOrdinalMemoAnchor: null,
        idOrdinalMemoStoredCount: null,
        idOrdinalMemoCanonicalCount: 0,
        requireResetReason: "cold_start",
        blockedUntilReset: false,
        seedPassPending: false,
        skipped: false,
        reseedAttempts: 0,
        lastReseedAttemptMs: null,
        reseedAwaitingSuccess: false,
        seedStartedAtMs: null,
        seedBudgetSpentMs: 0,
        resetReasons: [],
        sendFailureClass: null,
        consecutiveSendFailures: 0,
        parkedReason: null,
        parked: false,
        counters: emptyCounters(),
    };
}

function cloneJson<T>(value: T): T {
    return JSON.parse(JSON.stringify(value)) as T;
}

function yieldToEventLoop(): Promise<void> {
    return new Promise((resolve) => setTimeout(resolve, 0));
}

function stableHash(value: string): string {
    return createHmac("sha256", "magic-context-shadow-watermark").update(value).digest("hex");
}

function canonicalJson(value: unknown): string {
    if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
    if (isRecord(value)) {
        return `{${Object.keys(value)
            .sort()
            .map((key) => `${JSON.stringify(key)}:${canonicalJson(value[key])}`)
            .join(",")}}`;
    }
    const encoded = JSON.stringify(value);
    return encoded === undefined ? "null" : encoded;
}

function shadowTransformPageDigest(pageArrays: Record<string, unknown>): string {
    // Hash the JSON wire values, not in-memory undefined properties that JSON.stringify drops.
    const wireArrays = JSON.parse(JSON.stringify(pageArrays)) as Record<string, unknown>;
    return createHash("sha256").update(canonicalJson(wireArrays)).digest("hex");
}

function getMessageId(message: MessageLike): string | null {
    return typeof message.info.id === "string" && message.info.id.length > 0
        ? message.info.id
        : null;
}

function readStringField(part: unknown, field: TagNormalizationTarget["field"]): string | null {
    if (!isRecord(part)) return null;
    if (field === "text") return typeof part.text === "string" ? part.text : null;
    if (field === "tool_state_output") {
        return isRecord(part.state) && typeof part.state.output === "string"
            ? part.state.output
            : null;
    }
    return typeof part.content === "string" ? part.content : null;
}

function writeStringField(
    part: unknown,
    field: TagNormalizationTarget["field"],
    value: string,
): void {
    if (!isRecord(part)) return;
    if (field === "text") {
        part.text = value;
        return;
    }
    if (field === "tool_state_output") {
        if (isRecord(part.state)) part.state.output = value;
        return;
    }
    part.content = value;
}

function removeExactSuffix(value: string, suffix: string): string | null {
    if (!suffix || !value.endsWith(suffix)) return null;
    return value.slice(0, value.length - suffix.length);
}

export interface ShadowNormalizationRecord {
    kind: "tag_prefix" | "ctx_search_hint" | "summary_message";
    message_id: string | null;
    part_index: number;
    field: string;
    tag_number?: number;
    removed: string;
}

export function denormalizeShadowOutput(args: {
    db: ContextDatabase;
    sessionId: string;
    outputMessages: MessageLike[];
    normalizationTargets: readonly TagNormalizationTarget[];
}): { ts_output: unknown[]; normalizations: ShadowNormalizationRecord[] } {
    const clonedOutput = cloneJson(args.outputMessages) as unknown[];
    const normalizations: ShadowNormalizationRecord[] = [];
    const messageIndex = new Map<MessageLike, number>();
    args.outputMessages.forEach((message, index) => {
        messageIndex.set(message, index);
    });

    for (const target of args.normalizationTargets) {
        const msgIndex = messageIndex.get(target.message);
        if (msgIndex === undefined) continue;
        let partIndex = -1;
        for (let index = 0; index < target.message.parts.length; index += 1) {
            if (target.message.parts[index] === target.part) {
                partIndex = index;
                break;
            }
        }
        if (partIndex < 0) continue;
        const cloneMessage = clonedOutput[msgIndex] as { parts?: unknown[] } | undefined;
        const clonePart = cloneMessage?.parts?.[partIndex];
        const current = readStringField(clonePart, target.field);
        if (current === null) continue;
        const prefix = `§${target.tagNumber}§ `;
        if (!current.startsWith(prefix)) continue;
        writeStringField(clonePart, target.field, current.slice(prefix.length));
        normalizations.push({
            kind: "tag_prefix",
            message_id: getMessageId(target.message),
            part_index: partIndex,
            field: target.field,
            tag_number: target.tagNumber,
            removed: prefix,
        });
    }

    const hintDecisions = getAutoSearchHintDecisions(args.db, args.sessionId).filter(
        (decision) => decision.decision === "hint" && typeof decision.text === "string",
    );
    if (hintDecisions.length > 0) {
        const hintByMessage = new Map<string, string>();
        for (const decision of hintDecisions) {
            if (decision.decision === "hint") hintByMessage.set(decision.messageId, decision.text);
        }
        args.outputMessages.forEach((message, msgIndex) => {
            const messageId = getMessageId(message);
            if (!messageId) return;
            const hint = hintByMessage.get(messageId);
            if (!hint) return;
            const cloneMessage = clonedOutput[msgIndex] as { parts?: unknown[] } | undefined;
            for (let partIndex = 0; partIndex < message.parts.length; partIndex += 1) {
                const originalPart = message.parts[partIndex];
                if (!isRecord(originalPart) || originalPart.type !== "text") continue;
                const clonePart = cloneMessage?.parts?.[partIndex];
                const current = readStringField(clonePart, "text");
                if (current === null) continue;
                const stripped = removeExactSuffix(current, hint);
                if (stripped === null) continue;
                writeStringField(clonePart, "text", stripped);
                normalizations.push({
                    kind: "ctx_search_hint",
                    message_id: messageId,
                    part_index: partIndex,
                    field: "text",
                    removed: hint,
                });
                break;
            }
        });
    }

    const tsOutput = clonedOutput.filter((message, index) => {
        if (!isRawCompactionSummaryInfo(args.outputMessages[index]?.info)) return true;
        normalizations.push({
            kind: "summary_message",
            message_id: getMessageId(args.outputMessages[index]),
            part_index: -1,
            field: "ts_output",
            removed: JSON.stringify(message),
        });
        return false;
    });

    return { ts_output: tsOutput, normalizations };
}

export async function resolveOrdinalsForShadow(args: {
    sessionId: string;
    messages: MessageLike[];
    generation: number;
    memoGeneration: number;
    memo: Map<string, number>;
    memoAnchor?: RawMessageOrdinalAnchor | null;
    memoStoredCount?: number | null;
    memoCanonicalCount?: number;
}): Promise<Awaited<ReturnType<typeof resolveOrdinalsForModule>>> {
    return resolveOrdinalsForModule(args);
}

function flatBlockCountForRawMessage(message: RawMessageParts | undefined): number {
    if (!message) return 1;
    let count = 0;
    for (const part of message.parts) {
        if (!isRecord(part)) {
            count += 1;
            continue;
        }
        const type = typeof part.type === "string" ? part.type : "unknown";
        switch (type) {
            case "text":
                if (part.ignored !== true) count += 1;
                break;
            case "reasoning":
            case "file":
            case "image":
            case "step-start":
            case "subtask":
                count += 1;
                break;
            case "tool": {
                count += 1;
                const status = isRecord(part.state) ? part.state.status : undefined;
                const hasCompletedStatus = status === "completed" || status === "error";
                const hasOutput = isRecord(part.state)
                    ? typeof part.state.output === "string" || typeof part.state.error === "string"
                    : typeof part.output === "string" || typeof part.error === "string";
                if (hasCompletedStatus || hasOutput) count += 1;
                break;
            }
            case "compaction":
            case "step-finish":
            case "snapshot":
            case "patch":
            case "agent":
            case "retry":
                break;
            default:
                count += 1;
                break;
        }
    }
    return Math.max(1, count);
}

export function flatBlockIdForRawMessage(
    messageId: string,
    raw: RawMessageParts | undefined,
    edge: "start" | "end",
): string {
    const blockIndex = edge === "start" ? 0 : flatBlockCountForRawMessage(raw) - 1;
    return `${messageId}#${blockIndex}`;
}

export function resolveDeclaredTrimForShadow(args: {
    db: ContextDatabase;
    sessionId: string;
    state?: SessionQueueState;
}): ShadowDeclaredTrim | null {
    const marker = getPersistedCompactionMarkerState(args.db, args.sessionId);
    if (!marker || marker.boundaryOrdinal < 1) return null;
    const targetEndMessageId = marker.targetEndMessageId ?? marker.boundaryMessageId;
    if (!targetEndMessageId) return null;
    const markerKey = `${marker.boundaryOrdinal}:${targetEndMessageId}`;
    const cached = declaredTrimBySession.get(args.sessionId);
    if (cached?.markerKey === markerKey) return cached.trim;

    const boundaryRaw = readRawSessionMessagePartsById(args.sessionId, targetEndMessageId);
    if (!boundaryRaw) return null;
    const boundaryOrdinal = args.state
        ? canonicalOrdinalForMessageId({
              sessionId: args.sessionId,
              raw: boundaryRaw,
              messageId: targetEndMessageId,
              generation: args.state.shadowGeneration,
              state: args.state,
          })
        : readRawSessionMessageOrdinalById(args.sessionId, targetEndMessageId);
    if (boundaryOrdinal === null || boundaryOrdinal === "mismatch") return null;
    const compartments = getCompartmentsByEndMessageId(args.db, args.sessionId, targetEndMessageId);
    const boundaryCompartment = compartments.find(
        (compartment) => compartment.endMessage === marker.boundaryOrdinal,
    );
    if (!boundaryCompartment) return null;
    const trim: ShadowDeclaredTrim = {
        flat_boundary_id: flatBlockIdForRawMessage(targetEndMessageId, boundaryRaw, "end"),
        boundary_bare_message_id: targetEndMessageId,
        boundary_absolute_ordinal: boundaryOrdinal,
        next_absolute_ordinal: boundaryOrdinal + 1,
    };
    declaredTrimBySession.set(args.sessionId, { markerKey, trim });
    return trim;
}

function clearDeclaredTrimForSession(sessionId: string): void {
    declaredTrimBySession.delete(sessionId);
}

function didDeclaredTrimAdvance(
    before: ShadowDeclaredTrim | null,
    after: ShadowDeclaredTrim | null,
): boolean {
    if (!after) return false;
    if (!before) return true;
    return before.flat_boundary_id !== after.flat_boundary_id;
}

async function buildStateSyncPayload(args: {
    state: SessionQueueState;
    pass: Pick<ShadowTransformPass, "db" | "sessionId" | "projectPath" | "passInputs"> & {
        declaredTrim?: ShadowDeclaredTrim | null;
    };
    force: boolean;
    shouldAbortSeed?: () => boolean;
    beforeSerializeCompartment?: () => void;
    yieldEveryCompartments?: number;
    seedId?: string;
}): Promise<
    ShadowStateSyncPayload | null | "m0_mutation" | "mismatch" | "unresolved" | "seed_budget"
> {
    const payload = await buildModuleStateSyncPayload({
        state: args.state,
        pass: {
            db: args.pass.db,
            sessionId: args.pass.sessionId,
            projectPath: args.pass.projectPath,
            nowMs: args.pass.passInputs.now_ms,
        },
        force: args.force,
        seedId: args.seedId,
        options: {
            shouldAbortSeed: args.shouldAbortSeed,
            beforeSerializeCompartment: args.beforeSerializeCompartment,
            yieldEveryCompartments: args.yieldEveryCompartments,
        },
    });
    return payload as
        | ModuleStateSyncPayload
        | null
        | "m0_mutation"
        | "mismatch"
        | "unresolved"
        | "seed_budget";
}

function extractAckValue(response: unknown): Record<string, unknown> {
    if (isRecord(response) && isRecord(response.result))
        return response.result as Record<string, unknown>;
    return isRecord(response) ? response : {};
}

function numericAck(response: unknown, keys: string[], fallback: number): number {
    const value = extractAckValue(response);
    for (const key of keys) {
        const candidate = value[key];
        if (typeof candidate === "number" && Number.isFinite(candidate)) return candidate;
    }
    return fallback;
}

function errorCode(error: unknown): string | null {
    let current = error;
    const seen = new Set<unknown>();
    while (isRecord(current) && !seen.has(current)) {
        seen.add(current);
        if (typeof current.code === "string") return current.code;
        current = current.cause;
    }
    return null;
}

function isPeerReject(error: unknown): boolean {
    const code = errorCode(error);
    return (
        code === "stale_generation" ||
        code === "shadow_generation_mismatch" ||
        code === "shadow_seq_mismatch" ||
        code === "seq_mismatch" ||
        code === "cas_mismatch" ||
        code === "state_cas_reject"
    );
}

function deterministicSendFailureClass(error: unknown): string | null {
    const code = errorCode(error);
    if (
        code === "shadow_identity_drift" ||
        code === "identity_drift" ||
        code === "shadow_validation_reject" ||
        code === "invalid_params" ||
        code === "bad_shadow_input"
    ) {
        return code;
    }
    return null;
}

function isSeedBoundaryReject(error: unknown): boolean {
    return errorCode(error) === "shadow_seed_boundary_mismatch";
}

function isConnectionFailure(error: unknown): boolean {
    if (
        error instanceof SocketClosedError ||
        error instanceof SocketTimeoutError ||
        error instanceof StaleRouteHandleError ||
        isConsumerReconnectTransient(error)
    ) {
        return true;
    }
    const code = errorCode(error);
    return (
        code === "ENOENT" ||
        code === "ECONNREFUSED" ||
        code === "ECONNRESET" ||
        code === "EPIPE" ||
        code === "ETIMEDOUT" ||
        code === "request_deadline" ||
        code === "route_closed" ||
        code === "SUBC_CONNECTION_BACKOFF"
    );
}

function toFlatWireBody(payload: { method: string; params: Record<string, unknown> }): unknown {
    return toFlatModuleWireBody(payload);
}

function buildShadowResetBody(args: { state: SessionQueueState; reason: string }): {
    method: "shadow_reset";
    params: Record<string, unknown>;
} {
    return {
        method: "shadow_reset",
        params: {
            shadow_generation: args.state.shadowGeneration,
            reason: args.reason,
        },
    };
}

function buildShadowTransformBody(args: { pass: PreparedShadowPass; state: SessionQueueState }): {
    method: string;
    params: Record<string, unknown>;
} {
    const denormalized =
        args.pass.shadowTsOutput !== undefined && args.pass.shadowNormalizations !== undefined
            ? {
                  ts_output: args.pass.shadowTsOutput,
                  normalizations: args.pass.shadowNormalizations,
              }
            : denormalizeShadowOutput({
                  db: args.pass.db,
                  sessionId: args.pass.sessionId,
                  outputMessages: args.pass.outputMessages,
                  normalizationTargets: args.pass.normalizationTargets,
              });
    return {
        method: "shadow_transform",
        params: {
            shadow_generation: args.state.shadowGeneration,
            seed_pass: args.state.seedPassPending,
            input: args.pass.annotatedInput,
            ts_output: denormalized.ts_output,
            normalizations: denormalized.normalizations,
            pass_inputs: args.pass.passInputs,
            ts_decision: args.pass.tsDecision,
            declared_trim: args.pass.declaredTrim,
        },
    };
}

const SHADOW_TRANSFORM_ARRAY_FIELDS = [
    "input",
    "messages",
    "ts_output",
    "ts_ck_messages",
    "normalizations",
] as const;

function buildPagedTransformPayloads(body: Record<string, unknown>): Record<string, unknown>[] {
    return buildPagedModuleTransformPayloads(body);
}

export function createShadowSender(
    options: {
        transport?: ShadowTransport;
        now?: () => number;
        reseedCooldownMs?: number;
        reseedAttemptCap?: number;
        sendTimeoutMs?: number;
        queueMaxDepth?: number;
        seedBudgetMs?: number;
        seedClock?: () => number;
        beforeSerializeCompartment?: () => void;
        seedYieldEveryCompartments?: number;
        onSeedBudgetExceeded?: (message: string) => void;
    } = {},
): ShadowSender {
    const transport = options.transport ?? new SubcShadowTransport();
    const now = options.now ?? Date.now;
    const reseedCooldownMs = options.reseedCooldownMs ?? RESEED_COOLDOWN_MS;
    const reseedAttemptCap = options.reseedAttemptCap ?? RESEED_ATTEMPT_CAP;
    const sendTimeoutMs = Math.max(1, options.sendTimeoutMs ?? SHADOW_SEND_TIMEOUT_MS);
    const queueMaxDepth = Math.max(1, options.queueMaxDepth ?? SHADOW_QUEUE_MAX_DEPTH);
    const seedBudgetMs = Math.max(1, options.seedBudgetMs ?? SHADOW_SEED_BUDGET_MS);
    const seedClock = options.seedClock ?? (() => performance.now());
    const sessions = new Map<string, SessionQueueState>();
    const subagentSessions = new Set<string>();

    const getState = (sessionId: string): SessionQueueState => {
        let state = sessions.get(sessionId);
        if (!state) {
            state = createSessionQueueState();
            sessions.set(sessionId, state);
        }
        return state;
    };

    const isTransientResetReason = (reason: string): boolean =>
        reason === "route_reopen" ||
        reason === "transport_disconnect" ||
        reason === "daemon_restart" ||
        reason.includes("connection");

    const recordResetReason = (
        sessionId: string,
        state: SessionQueueState,
        reason: string,
    ): boolean => {
        if (state.parked) return false;
        const previous = state.resetReasons.at(-1);
        state.resetReasons.push(reason);
        if (state.resetReasons.length > SHADOW_RESET_REASON_RING_SIZE) {
            state.resetReasons.shift();
        }
        if (isTransientResetReason(reason) || previous !== reason) return true;
        state.parked = true;
        state.parkedReason = "reset_repeat";
        state.queue.length = 0;
        state.seedStartedAtMs = null;
        state.seedBudgetSpentMs = 0;
        state.reseedAwaitingSuccess = false;
        state.blockedUntilReset = false;
        state.requireResetReason = null;
        state.seedPassPending = false;
        state.counters.parked += 1;
        transport.closeSession?.(sessionId);
        sessionLog(sessionId, `shadow: parked (repeated ${reason})`);
        return false;
    };

    const recordDeterministicSendFailure = (
        sessionId: string,
        state: SessionQueueState,
        failureClass: string,
    ): boolean => {
        if (state.sendFailureClass === failureClass) {
            state.consecutiveSendFailures += 1;
        } else {
            state.sendFailureClass = failureClass;
            state.consecutiveSendFailures = 1;
        }
        if (state.consecutiveSendFailures < SHADOW_SEND_FAILURE_PARK_THRESHOLD) return true;
        state.parked = true;
        state.parkedReason = "send_failure";
        state.queue.length = 0;
        state.seedStartedAtMs = null;
        state.seedBudgetSpentMs = 0;
        state.reseedAwaitingSuccess = false;
        state.blockedUntilReset = false;
        state.requireResetReason = null;
        state.seedPassPending = false;
        state.counters.parked += 1;
        transport.closeSession?.(sessionId);
        sessionLog(sessionId, `shadow: parked (repeated send failure ${failureClass})`);
        return false;
    };

    const remainingSeedBudgetMs = (state: SessionQueueState): number => {
        const activeElapsed =
            state.seedStartedAtMs === null ? 0 : Math.max(0, seedClock() - state.seedStartedAtMs);
        return seedBudgetMs - state.seedBudgetSpentMs - activeElapsed;
    };

    const markResetRequired = (
        sessionId: string,
        state: SessionQueueState,
        reason: string,
    ): void => {
        transport.closeSession?.(sessionId);
        state.initialized = false;
        state.blockedUntilReset = true;
        state.requireResetReason = reason;
        state.lastAckedWatermarks = null;
        state.seedPassPending = true;
    };

    const requireSeedBudget = (state: SessionQueueState): number => {
        const remaining = remainingSeedBudgetMs(state);
        if (remaining > 0) return remaining;
        const error = new Error("shadow seed budget exhausted") as Error & { code?: string };
        error.code = "shadow_seed_budget";
        throw error;
    };

    const disableIfSeedBudgetExceeded = (sessionId: string, state: SessionQueueState): boolean => {
        if (remainingSeedBudgetMs(state) > 0) return false;
        markResetRequired(sessionId, state, "seed_budget");
        if (!state.skipped) {
            state.skipped = true;
            state.queue.length = 0;
            state.counters.seed_budget_exceeded += 1;
            const message = "shadow: seed budget exceeded, lane disabled for session";
            sessionLog(sessionId, message);
            options.onSeedBudgetExceeded?.(message);
        }
        return true;
    };

    const pauseSeedBudget = (state: SessionQueueState): void => {
        if (state.seedStartedAtMs === null) return;
        state.seedBudgetSpentMs += Math.max(0, seedClock() - state.seedStartedAtMs);
        state.seedStartedAtMs = null;
    };

    const schedule = (sessionId: string): void => {
        const state = getState(sessionId);
        if (state.running || state.parked) return;
        state.running = true;
        void runQueue(sessionId, state).finally(() => {
            state.running = false;
            if (!state.skipped && !state.parked && state.queue.length > 0) schedule(sessionId);
        });
    };

    const pushWork = (sessionId: string, work: ShadowWorkItem): void => {
        const state = getState(sessionId);
        if (state.skipped || state.parked) return;
        let dropped = 0;
        if (work.kind === "pass") {
            state.counters.enqueued += 1;
            // A queued pass is also the pending state-sync/seed unit: its large
            // payload is built only after dequeue. Superseding the whole pass here
            // guarantees that a stalled send can retain at most one newer snapshot.
            for (let index = state.queue.length - 1; index >= 0; index -= 1) {
                if (state.queue[index]?.kind !== "pass") continue;
                state.queue.splice(index, 1);
                dropped += 1;
            }
        }
        state.queue.push(work);
        while (state.queue.length > queueMaxDepth) {
            const oldestNonEssential = state.queue.findIndex(
                (item, index) => item.kind === "pass" && index < state.queue.length - 1,
            );
            state.queue.splice(oldestNonEssential >= 0 ? oldestNonEssential : 0, 1);
            dropped += 1;
        }
        // Shadow comparison is best-effort instrumentation. A counter is enough
        // to diagnose pressure without turning each dropped pass into log traffic.
        state.counters.dropped_oldest += dropped;
        schedule(sessionId);
    };

    const callTransport = async (
        state: SessionQueueState,
        args: Parameters<ShadowTransport["call"]>[0],
        timeoutCapMs = sendTimeoutMs,
    ): Promise<unknown> => {
        const controller = new AbortController();
        const requestTimeoutMs = Math.max(1, Math.min(sendTimeoutMs, Math.floor(timeoutCapMs)));
        let timer: ReturnType<typeof setTimeout> | undefined;
        const timeout = new Promise<never>((_, reject) => {
            timer = setTimeout(() => {
                state.counters.send_timeouts += 1;
                const error = new Error(
                    `shadow send timeout after ${requestTimeoutMs}ms`,
                ) as Error & { code?: string };
                error.code = "ETIMEDOUT";
                controller.abort(error);
                reject(error);
            }, requestTimeoutMs);
        });
        try {
            return await Promise.race([
                transport.call({ ...args, signal: controller.signal }),
                timeout,
            ]);
        } finally {
            if (timer) clearTimeout(timer);
        }
    };

    const runQueue = async (sessionId: string, state: SessionQueueState): Promise<void> => {
        while (!state.skipped && !state.parked && state.queue.length > 0) {
            const item = state.queue.shift();
            if (!item) continue;
            if (item.kind === "reset") {
                try {
                    await performReset({
                        sessionId,
                        state,
                        reason: item.reason,
                        projectRoot: item.projectRoot,
                    });
                    pauseSeedBudget(state);
                    disableIfSeedBudgetExceeded(sessionId, state);
                } catch (error) {
                    state.counters.send_failures += 1;
                    markResetRequired(sessionId, state, item.reason || "reset_retry");
                    sessionLog(sessionId, "shadow: reset failed (ignored):", error);
                }
                continue;
            }
            try {
                await processPass(state, item.pass);
                state.sendFailureClass = null;
                state.consecutiveSendFailures = 0;
            } catch (error) {
                state.counters.send_failures += 1;
                const failureClass = deterministicSendFailureClass(error);
                if (failureClass !== null) {
                    if (!recordDeterministicSendFailure(sessionId, state, failureClass)) continue;
                } else {
                    state.sendFailureClass = null;
                    state.consecutiveSendFailures = 0;
                }
                if (isPeerReject(error)) {
                    const code = errorCode(error);
                    if (code?.includes("generation")) state.counters.generation_rejects += 1;
                    else state.counters.state_sync_rejects += 1;
                    state.requireResetReason = code ?? "peer_reject";
                    state.blockedUntilReset = true;
                    state.lastAckedWatermarks = null;
                } else if (isConnectionFailure(error)) {
                    state.counters.connection_skips += 1;
                    state.initialized = false;
                    state.requireResetReason = "route_reopen";
                    if (state.reseedAwaitingSuccess) {
                        // A transport failure did not establish a fresh lineage, so it
                        // must not consume the reseed cooldown or attempt allowance.
                        state.reseedAttempts = Math.max(0, state.reseedAttempts - 1);
                        state.lastReseedAttemptMs = null;
                        state.reseedAwaitingSuccess = false;
                    }
                }
                sessionLog(sessionId, "shadow: send failed (ignored):", error);
            }
            pauseSeedBudget(state);
            disableIfSeedBudgetExceeded(sessionId, state);
        }
    };

    const performReset = async (args: {
        sessionId: string;
        state: SessionQueueState;
        reason: string;
        projectRoot?: string;
    }): Promise<void> => {
        if (!recordResetReason(args.sessionId, args.state, args.reason)) return;
        args.state.seedBudgetSpentMs = 0;
        args.state.seedStartedAtMs = seedClock();
        const projectRoot = args.projectRoot ?? process.cwd();
        const body = toFlatWireBody(buildShadowResetBody(args));
        const response = await callTransport(
            args.state,
            {
                sessionId: args.sessionId,
                projectRoot,
                method: "shadow_reset",
                body,
            },
            requireSeedBudget(args.state),
        );
        requireSeedBudget(args.state);
        args.state.shadowGeneration = numericAck(
            response,
            ["shadow_generation", "generation"],
            args.state.shadowGeneration + 1,
        );
        args.state.lastAckedSeq = numericAck(response, ["shadow_seq", "seq"], 0);
        args.state.lastAckedWatermarks = null;
        args.state.idOrdinalMemo.clear();
        args.state.idOrdinalMemoGeneration = args.state.shadowGeneration;
        args.state.idOrdinalMemoAnchor = null;
        args.state.idOrdinalMemoStoredCount = null;
        args.state.idOrdinalMemoCanonicalCount = 0;
        clearDeclaredTrimForSession(args.sessionId);
        args.state.initialized = true;
        args.state.blockedUntilReset = false;
        args.state.requireResetReason = null;
        // Every fresh lineage must commit one normal transform before byte comparison.
        // The complete sync makes source state available; the seed pass then establishes
        // the shadow lane's own first-render cache and boundary state.
        args.state.seedPassPending = true;
        args.state.sendFailureClass = null;
        args.state.consecutiveSendFailures = 0;
        args.state.counters.resets_sent += 1;
        sessionLog(
            args.sessionId,
            `shadow: reset acknowledged (generation=${args.state.shadowGeneration}, reason=${args.reason})`,
        );
    };

    const beginReseed = (state: SessionQueueState): boolean => {
        const attemptAt = now();
        if (state.reseedAttempts >= reseedAttemptCap) return false;
        if (
            state.lastReseedAttemptMs !== null &&
            attemptAt - state.lastReseedAttemptMs < reseedCooldownMs
        ) {
            return false;
        }
        state.reseedAttempts += 1;
        state.lastReseedAttemptMs = attemptAt;
        state.reseedAwaitingSuccess = true;
        return true;
    };

    const processPass = async (
        state: SessionQueueState,
        pass: ShadowTransformPass,
    ): Promise<void> => {
        if (state.skipped || state.parked) return;
        if (state.seedPassPending && state.seedStartedAtMs === null) {
            state.seedStartedAtMs = seedClock();
        }
        if (state.blockedUntilReset && !state.requireResetReason) {
            state.requireResetReason = "unknown_block";
        }
        if (!state.initialized || state.requireResetReason) {
            await performReset({
                sessionId: pass.sessionId,
                state,
                reason: state.requireResetReason ?? "cold_start",
                projectRoot: pass.projectRoot,
            });
        }
        if (state.skipped || state.blockedUntilReset) return;
        if (disableIfSeedBudgetExceeded(pass.sessionId, state)) return;

        let resolved: Awaited<ReturnType<typeof resolveOrdinalsForShadow>>;
        try {
            resolved = await resolveOrdinalsForShadow({
                sessionId: pass.sessionId,
                messages: pass.inputMessages,
                generation: state.shadowGeneration,
                memoGeneration: state.idOrdinalMemoGeneration,
                memo: state.idOrdinalMemo,
                memoAnchor: state.idOrdinalMemoAnchor,
                memoStoredCount: state.idOrdinalMemoStoredCount,
                memoCanonicalCount: state.idOrdinalMemoCanonicalCount,
            });
        } catch (error) {
            if (state.seedPassPending)
                markResetRequired(pass.sessionId, state, "seed_capture_failed");
            sessionLog(pass.sessionId, "shadow: capture failed (ignored):", error);
            return;
        }
        if (!resolved.ok) {
            if (resolved.reason === "mismatch") {
                state.counters.ordinal_mismatch += 1;
                state.idOrdinalMemo.clear();
                if (state.seedPassPending) {
                    // Without this line a persistent seed-pass mismatch loops reset,
                    // seed, mismatch silently; the ack lines alone look healthy.
                    sessionLog(
                        pass.sessionId,
                        "shadow: seed pass ordinal mismatch, reset re-armed",
                    );
                    markResetRequired(pass.sessionId, state, "ordinal_mismatch");
                } else {
                    await performReset({
                        sessionId: pass.sessionId,
                        state,
                        reason: "ordinal_mismatch",
                        projectRoot: pass.projectRoot,
                    });
                    if (!state.skipped) await processPass(state, pass);
                }
            } else {
                state.counters.ordinal_unresolved += 1;
                if (state.seedPassPending)
                    markResetRequired(pass.sessionId, state, "seed_ordinal_unresolved");
                sessionLog(
                    pass.sessionId,
                    `shadow: pass skipped; unresolved ordinal for ${resolved.messageId ?? "unknown"}`,
                );
            }
            return;
        }
        state.idOrdinalMemoGeneration = resolved.memoGeneration;
        state.idOrdinalMemoAnchor = resolved.memoAnchor;
        state.idOrdinalMemoStoredCount = resolved.memoStoredCount;
        state.idOrdinalMemoCanonicalCount = resolved.memoCanonicalCount;
        if (disableIfSeedBudgetExceeded(pass.sessionId, state)) return;

        let preparedPass: PreparedShadowPass;
        let syncPayload: Awaited<ReturnType<typeof buildStateSyncPayload>>;
        try {
            const declaredTrim = resolveDeclaredTrimForShadow({
                db: pass.db,
                sessionId: pass.sessionId,
                state,
            });
            const denormalized = denormalizeShadowOutput({
                db: pass.db,
                sessionId: pass.sessionId,
                outputMessages: pass.outputMessages,
                normalizationTargets: pass.normalizationTargets,
            });
            preparedPass = {
                ...pass,
                annotatedInput: resolved.annotatedInput,
                shadowTsOutput: denormalized.ts_output,
                shadowNormalizations: [...resolved.normalizations, ...denormalized.normalizations],
                declaredTrim,
                tsDecision: {
                    ...pass.tsDecision,
                    marker_state: {
                        marker_message_id: declaredTrim?.boundary_bare_message_id,
                        advanced_this_pass: didDeclaredTrimAdvance(
                            pass.declaredTrimBefore,
                            declaredTrim,
                        ),
                    },
                },
            };
            syncPayload = await buildStateSyncPayload({
                state,
                pass: preparedPass,
                force: state.lastAckedWatermarks === null,
                shouldAbortSeed: state.seedPassPending
                    ? () =>
                          state.seedBudgetSpentMs +
                              (state.seedStartedAtMs === null
                                  ? 0
                                  : Math.max(0, seedClock() - state.seedStartedAtMs)) >
                          seedBudgetMs
                    : undefined,
                beforeSerializeCompartment: options.beforeSerializeCompartment,
                yieldEveryCompartments: options.seedYieldEveryCompartments,
            });
        } catch (error) {
            if (state.seedPassPending)
                markResetRequired(pass.sessionId, state, "seed_capture_failed");
            sessionLog(pass.sessionId, "shadow: capture failed (ignored):", error);
            return;
        }

        if (syncPayload === "m0_mutation") {
            await performReset({
                sessionId: pass.sessionId,
                state,
                reason: "m0_mutation",
                projectRoot: pass.projectRoot,
            });
            try {
                const fullSync = await buildStateSyncPayload({
                    state,
                    pass: preparedPass,
                    force: true,
                    shouldAbortSeed: () =>
                        state.seedBudgetSpentMs +
                            (state.seedStartedAtMs === null
                                ? 0
                                : Math.max(0, seedClock() - state.seedStartedAtMs)) >
                        seedBudgetMs,
                    beforeSerializeCompartment: options.beforeSerializeCompartment,
                    yieldEveryCompartments: options.seedYieldEveryCompartments,
                });
                if (fullSync === "m0_mutation") {
                    throw new Error("forced state sync unexpectedly requested another m0 reset");
                }
                syncPayload = fullSync;
            } catch (error) {
                markResetRequired(pass.sessionId, state, "seed_capture_failed");
                sessionLog(pass.sessionId, "shadow: capture failed (ignored):", error);
                return;
            }
        }
        if (syncPayload === "seed_budget") {
            disableIfSeedBudgetExceeded(pass.sessionId, state);
            return;
        }
        if (syncPayload === "mismatch") {
            state.counters.ordinal_mismatch += 1;
            markResetRequired(pass.sessionId, state, "ordinal_mismatch");
            return;
        }
        if (syncPayload === "unresolved") {
            state.counters.ordinal_unresolved += 1;
            if (state.seedPassPending)
                markResetRequired(pass.sessionId, state, "seed_compartment_unresolved");
            sessionLog(
                pass.sessionId,
                "shadow: state sync skipped; compartment ordinal unresolved",
            );
            return;
        }
        if (
            syncPayload !== null &&
            !syncPayload.wireBatches &&
            flatWireBodyBytes(syncPayload) >= MAX_FACADE_FRAME_BYTES
        ) {
            await performReset({
                sessionId: pass.sessionId,
                state,
                reason: "oversized_state_sync",
                projectRoot: pass.projectRoot,
            });
            try {
                const pagedSeed = await buildStateSyncPayload({
                    state,
                    pass: preparedPass,
                    force: true,
                    shouldAbortSeed: () => remainingSeedBudgetMs(state) <= 0,
                    beforeSerializeCompartment: options.beforeSerializeCompartment,
                    yieldEveryCompartments: options.seedYieldEveryCompartments,
                });
                if (
                    pagedSeed === null ||
                    pagedSeed === "m0_mutation" ||
                    pagedSeed === "mismatch" ||
                    pagedSeed === "unresolved"
                ) {
                    throw new Error(`forced paged seed failed: ${String(pagedSeed)}`);
                }
                if (pagedSeed === "seed_budget") {
                    disableIfSeedBudgetExceeded(pass.sessionId, state);
                    return;
                }
                syncPayload = pagedSeed;
            } catch (error) {
                markResetRequired(pass.sessionId, state, "seed_rebuild_failed");
                throw error;
            }
        }
        if (syncPayload !== null) {
            const wireBatches = syncPayload.wireBatches ?? [syncPayload];
            const pagedSeed = syncPayload.wireBatches !== undefined;
            let response: unknown;
            try {
                for (let index = 0; index < wireBatches.length; index += 1) {
                    const batch = wireBatches[index];
                    const timeoutCap = pagedSeed ? requireSeedBudget(state) : sendTimeoutMs;
                    response = await callTransport(
                        state,
                        {
                            sessionId: pass.sessionId,
                            projectRoot: pass.projectRoot,
                            method: "state_sync",
                            body: toFlatWireBody(batch),
                        },
                        timeoutCap,
                    );
                    if (pagedSeed) requireSeedBudget(state);
                    if (index + 1 < wireBatches.length) continue;
                    state.lastAckedSeq = numericAck(
                        response,
                        ["shadow_seq", "seq"],
                        batch.params.expected_shadow_seq + 1,
                    );
                    state.lastAckedWatermarks = syncPayload.watermarks;
                }
            } catch (error) {
                if (pagedSeed) {
                    markResetRequired(
                        pass.sessionId,
                        state,
                        errorCode(error) ?? "seed_batch_failed",
                    );
                }
                if (isSeedBoundaryReject(error) && beginReseed(state)) {
                    await performReset({
                        sessionId: pass.sessionId,
                        state,
                        reason: "seed_boundary_reseed",
                        projectRoot: pass.projectRoot,
                    });
                    if (!state.skipped) await processPass(state, pass);
                    return;
                }
                throw error;
            }
            if (state.skipped) return;
            if (pagedSeed) {
                state.seedStartedAtMs = null;
                state.seedBudgetSpentMs = 0;
            }
        }

        const transformBody = toFlatWireBody(
            buildShadowTransformBody({ pass: preparedPass, state }),
        ) as Record<string, unknown>;
        const transformPages = buildPagedTransformPayloads(transformBody);
        let response: unknown;
        for (const page of transformPages) {
            response = await callTransport(state, {
                sessionId: pass.sessionId,
                projectRoot: pass.projectRoot,
                method: "shadow_transform",
                body: page,
            });
        }
        if (state.skipped || state.parked) return;
        const ack = extractAckValue(response);
        state.seedPassPending = false;
        state.counters.transforms_sent += 1;
        if (ack.divergence || ack.divergence_class || ack.hard_divergence) {
            sessionLog(pass.sessionId, "shadow: divergence report", ack);
        }
        if (ack.quarantined === true && beginReseed(state)) {
            await performReset({
                sessionId: pass.sessionId,
                state,
                reason: "quarantine_reseed",
                projectRoot: pass.projectRoot,
            });
            if (!state.skipped) await processPass(state, pass);
        } else if (ack.quarantined !== true && state.reseedAwaitingSuccess) {
            state.reseedAttempts = 0;
            state.lastReseedAttemptMs = null;
            state.reseedAwaitingSuccess = false;
        }
    };

    return {
        enqueue(pass: ShadowTransformPass): void {
            if (pass.isSubagent) {
                const state = sessions.get(pass.sessionId);
                if (state) {
                    state.skipped = true;
                    state.queue.length = 0;
                    sessions.delete(pass.sessionId);
                }
                clearDeclaredTrimForSession(pass.sessionId);
                transport.closeSession?.(pass.sessionId);
                if (!subagentSessions.has(pass.sessionId)) {
                    subagentSessions.add(pass.sessionId);
                    sessionLog(pass.sessionId, "shadow: skipped (subagent session)");
                }
                return;
            }
            pushWork(pass.sessionId, { kind: "pass", pass });
        },
        resetSession(sessionId: string, reason: string): void {
            if (subagentSessions.has(sessionId)) return;
            const state = getState(sessionId);
            if (state.parked) {
                if (state.parkedReason !== "send_failure") return;
                // An explicit reset is the recovery boundary for a deterministic reject:
                // the module drops the poisoned lineage and the sender may seed it again.
                state.parked = false;
                state.parkedReason = null;
                state.resetReasons = [];
                state.sendFailureClass = null;
                state.consecutiveSendFailures = 0;
            }
            state.queue.length = 0;
            state.requireResetReason = reason;
            state.blockedUntilReset = true;
            transport.closeSession?.(sessionId);
            pushWork(sessionId, { kind: "reset", sessionId, reason });
        },
        clearSession(sessionId: string): void {
            const state = sessions.get(sessionId);
            if (state) state.skipped = true;
            sessions.delete(sessionId);
            subagentSessions.delete(sessionId);
            clearDeclaredTrimForSession(sessionId);
            transport.closeSession?.(sessionId);
        },
        getStats(sessionId: string): Readonly<ShadowSenderCounters> {
            return { ...getState(sessionId).counters };
        },
        getQueueDepth(sessionId: string): number {
            return getState(sessionId).queue.length;
        },
    };
}

export class SubcShadowTransport implements ShadowTransport {
    private readonly connectionFile: string;
    private readonly moduleId: string;
    private readonly requestTimeoutMs: number;
    private readonly routeSessionPrefix: string;
    private client: SubcClient | null = null;
    private routes = new Map<string, RouteHandle>();
    private activeSession: string | null = null;
    private nextProbeMs = 0;
    private authorityProjectRoot = "";
    /**
     * Filesystem root used to bind authority/mirror routes. Authority request
     * bodies carry the MC project IDENTITY (git:<sha> / dir:<hash>), which is not
     * a path — the daemon validates BindIdentity.project_root against the real
     * filesystem and rejects identity strings outright.
     */
    private authorityBindRoot = "";
    private backoffMs = CONNECT_BACKOFF_INITIAL_MS;
    private connectionGeneration = 0;

    constructor(
        connectionFile?: string,
        moduleId = DEFAULT_MODULE_ID,
        requestTimeoutMs = SHADOW_SEND_TIMEOUT_MS,
        routeSessionPrefix = "shadow:",
    ) {
        this.connectionFile = connectionFile ?? getDefaultConnectionFile();
        this.moduleId = moduleId;
        this.requestTimeoutMs = requestTimeoutMs;
        this.routeSessionPrefix = routeSessionPrefix;
    }

    async call(args: {
        sessionId: string;
        projectRoot: string;
        method:
            | "shadow_reset"
            | "state_sync"
            | "shadow_transform"
            | "transform"
            | "session.status"
            | "session.flush"
            | "session.recomp"
            | "session.wrapup"
            | "todo_state.set"
            | "agent_drops.append"
            | "authority.status"
            | "authority.prepare"
            | "authority.seed"
            | "authority.drain.begin"
            | "authority.drain.finish"
            | "authority.drain_seed"
            | "authority.drain_memories"
            | "authority.drain_notes"
            | "authority.drain_compartments"
            | "authority.drain_reconcile"
            | "authority.drain_verify"
            | "authority.drain_flip"
            | "authority.drain_finish"
            | "mirror.pull"
            | "ctx_note"
            | "ctx_memory"
            | "note.evaluate"
            | "transform.ack"
            | "transform.nack";
        body: unknown;
        signal?: AbortSignal;
    }): Promise<unknown> {
        // Each waiting closure would retain its complete shadow payload. Rejecting
        // concurrent best-effort work keeps transport memory bounded.
        if (this.activeSession !== null) {
            const error = new Error("shadow transport busy; work dropped") as Error & {
                code?: string;
            };
            error.code = "EBUSY";
            throw error;
        }
        if (args.signal?.aborted) throw args.signal.reason;

        this.activeSession = args.sessionId;
        const onAbort = () => this.invalidateConnection();
        args.signal?.addEventListener("abort", onAbort, { once: true });
        try {
            const { client, route } = await this.ensureRoute(args.sessionId, args.projectRoot);
            if (args.signal?.aborted) throw args.signal.reason;
            return await client.request(route, args.body, {
                priority: Priority.Background,
                admissionClass: AdmissionClass.Normal,
                timeoutMs: this.requestTimeoutMs,
            });
        } catch (error) {
            if (isConnectionFailure(error)) this.invalidateConnection();
            throw error;
        } finally {
            args.signal?.removeEventListener("abort", onAbort);
            this.activeSession = null;
        }
    }

    private async authorityRequest(
        sessionId: string,
        projectRoot: string,
        method:
            | "authority.status"
            | "authority.prepare"
            | "authority.seed"
            | "authority.drain.begin"
            | "authority.drain.finish"
            | "authority.drain_seed"
            | "authority.drain_memories"
            | "authority.drain_notes"
            | "authority.drain_compartments"
            | "authority.drain_reconcile"
            | "authority.drain_verify"
            | "authority.drain_finish"
            | "mirror.pull",
        body: Record<string, unknown>,
    ): Promise<Record<string, unknown>> {
        // The transport serializes the body verbatim; the module dispatches on the
        // body's own method field, so it must always be present and canonical here.
        const response = (await this.call({
            sessionId,
            projectRoot,
            method,
            body: { ...body, method, v: 1 },
        })) as unknown;
        if (isRecord(response) && isRecord(response.result)) return response.result;
        if (isRecord(response)) return response;
        throw new Error(`module returned an invalid ${method} response`);
    }

    setAuthorityBindRoot(root: string): void {
        this.authorityBindRoot = root;
    }

    private bindRootForAuthority(): string {
        return this.authorityBindRoot.length > 0 ? this.authorityBindRoot : process.cwd();
    }

    async authorityStatus(args: {
        context_store_uuid: string;
        project: string;
        domain: "memories" | "notes";
    }): Promise<{ authority: AuthorityStatus | null }> {
        this.authorityProjectRoot = args.project;
        const response = await this.authorityRequest(
            args.project,
            this.bindRootForAuthority(),
            "authority.status",
            args,
        );
        return { authority: (response.authority as AuthorityStatus | null) ?? null };
    }

    async authorityPrepare(args: Record<string, unknown>): Promise<{ authority: AuthorityStatus }> {
        this.authorityProjectRoot = String(args.project ?? "");
        const response = await this.authorityRequest(
            String(args.project ?? "authority"),
            this.bindRootForAuthority(),
            "authority.prepare",
            args,
        );
        if (!isRecord(response.authority)) throw new Error("authority.prepare omitted authority");
        return { authority: response.authority as unknown as AuthorityStatus };
    }

    async authoritySeed(
        args: Record<string, unknown>,
    ): Promise<{ seeded: number; module_row_ids?: number[] }> {
        this.authorityProjectRoot = String(args.project ?? "");
        const response = await this.authorityRequest(
            String(args.project ?? "authority"),
            this.bindRootForAuthority(),
            "authority.seed",
            args,
        );
        return {
            seeded: typeof response.seeded === "number" ? response.seeded : 0,
            module_row_ids: Array.isArray(response.module_row_ids)
                ? response.module_row_ids.filter((id): id is number => typeof id === "number")
                : undefined,
        };
    }

    async authorityDrain(args: Record<string, unknown>): Promise<{ authority: AuthorityStatus }> {
        this.authorityProjectRoot = String(args.project ?? this.authorityProjectRoot);
        const method = String(args.method ?? "authority.drain.step") as Parameters<
            SubcShadowTransport["authorityRequest"]
        >[2];
        const response = await this.authorityRequest(
            String(args.project ?? "authority"),
            this.bindRootForAuthority(),
            method,
            args,
        );
        if (!isRecord(response.authority)) throw new Error("authority.drain omitted authority");
        return { authority: response.authority as unknown as AuthorityStatus };
    }

    async mirrorPull(args: {
        domain: "memories" | "notes";
        cursor: number;
        limit: number;
    }): Promise<{ page: ChangefeedPage }> {
        const response = await this.authorityRequest(
            `mirror:${args.domain}`,
            this.bindRootForAuthority(),
            "mirror.pull",
            args,
        );
        if (!isRecord(response.page)) throw new Error("mirror.pull omitted page");
        return { page: response.page as unknown as ChangefeedPage };
    }

    closeSession(sessionId: string): void {
        const route = this.routes.get(sessionId);
        this.routes.delete(sessionId);
        const client = this.client;
        if (route && client) {
            void client.closeRoute(route).catch((error: unknown) => {
                if (this.client === client && isConnectionFailure(error)) {
                    this.invalidateConnection(client);
                }
            });
            return;
        }
        if (this.activeSession === sessionId) this.invalidateConnection(client);
    }

    private async ensureRoute(
        sessionId: string,
        projectRoot: string,
    ): Promise<{ client: SubcClient; route: RouteHandle }> {
        const existing = this.routes.get(sessionId);
        const client = await this.ensureConnected();
        if (existing) return { client, route: existing };

        const target: RouteTarget = { kind: "tool_provider", module_id: this.moduleId };
        const identity: BindIdentity = {
            project_root: projectRoot,
            harness: getHarness(),
            session: `${this.routeSessionPrefix}${sessionId}`,
        };
        const route = await client.routeOpen(target, identity);
        if (this.client !== client) {
            await client.closeRoute(route).catch(() => undefined);
            const error = new Error(
                "subc connection changed while opening shadow route",
            ) as Error & {
                code?: string;
            };
            error.code = "ECONNRESET";
            throw error;
        }
        this.routes.set(sessionId, route);
        return { client, route };
    }

    private async ensureConnected(): Promise<SubcClient> {
        if (this.client) return this.client;
        const now = Date.now();
        if (now < this.nextProbeMs) {
            const error = new Error(
                `subc connection backoff active until ${this.nextProbeMs}`,
            ) as Error & {
                code?: string;
            };
            error.code = "SUBC_CONNECTION_BACKOFF";
            throw error;
        }

        const generation = this.connectionGeneration;
        let candidate: SubcClient | null = null;
        try {
            candidate = await SubcClient.connect({
                connectionFile: this.connectionFile,
                handshakeTimeoutMs: HANDSHAKE_TIMEOUT_MS,
            });
            if (generation !== this.connectionGeneration) {
                candidate.close();
                const error = new Error("subc connection attempt was superseded") as Error & {
                    code?: string;
                };
                error.code = "ECONNRESET";
                throw error;
            }
            this.client = candidate;
            this.routes.clear();
            this.backoffMs = CONNECT_BACKOFF_INITIAL_MS;
            this.nextProbeMs = 0;
            return candidate;
        } catch (error) {
            candidate?.close();
            this.invalidateConnection();
            this.nextProbeMs = Date.now() + this.backoffMs;
            this.backoffMs = Math.min(this.backoffMs * 2, CONNECT_BACKOFF_MAX_MS);
            throw error;
        }
    }

    private invalidateConnection(client: SubcClient | null = this.client): void {
        if (client && this.client && client !== this.client) return;
        this.connectionGeneration += 1;
        this.client = null;
        this.routes.clear();
        client?.close();
    }
}

export const __shadowSenderTest = {
    MAX_FACADE_FRAME_BYTES,
    SHADOW_SEED_BATCH_MAX_BYTES,
    SHADOW_TRANSFORM_PAGE_MAX_BYTES,
    SubcShadowTransport,
    buildPagedSeedPayloads,
    buildShadowResetBody,
    buildShadowTransformBody,
    buildPagedTransformPayloads,
    buildStateSyncPayload,
    flatWireBodyBytes,
    createSessionQueueState,
    denormalizeShadowOutput,
    flatBlockIdForRawMessage,
    resolveDeclaredTrimForShadow,
    resolveOrdinalsForShadow,
    toFlatWireBody,
};
