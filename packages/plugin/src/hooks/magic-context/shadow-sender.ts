import { createHmac, randomBytes, timingSafeEqual } from "node:crypto";
import { existsSync, readFileSync, statSync } from "node:fs";
import net from "node:net";
import { join } from "node:path";
import { getCompartmentsByEndMessageId } from "../../features/magic-context/compartment-storage";
import {
    getMaxMemoryIdForProjects,
    getMemoriesByProject,
    getMemoriesByProjects,
} from "../../features/magic-context/memory/storage-memory";
import {
    type ContextDatabase,
    getCompartments,
    getOrCreateSessionMeta,
} from "../../features/magic-context/storage";
import {
    getMaxMemoryMutationIdForProjects,
    getMemoryMutationsForRenderByProjects,
} from "../../features/magic-context/storage-memory-mutation-log";
import {
    getAutoSearchHintDecisions,
    getPersistedCompactionMarkerState,
} from "../../features/magic-context/storage-meta-persisted";
import {
    computeWorkspaceEpochFingerprint,
    expandWorkspaceIdentitySetWithAliases,
    resolveWorkspaceIdentitySet,
    resolveWorkspaceShareCategories,
} from "../../features/magic-context/workspaces";
import { getDataDir } from "../../shared/data-path";
import { getHarness } from "../../shared/harness";
import { sessionLog } from "../../shared/logger";
import { isRecord } from "../../shared/record-type-guard";
import {
    readRawSessionMessageById,
    readRawSessionMessages,
    withRawSessionMessageCache,
} from "./read-session-chunk";
import type { RawMessage } from "./read-session-raw";
import type { MessageLike, TagNormalizationTarget } from "./tag-messages";
import { formatDate } from "./temporal-awareness";

const DEFAULT_MODULE_ID = "magic-context";
function getDefaultConnectionFile(): string {
    return join(getDataDir(), "cortexkit", "run", "subc-connection.json");
}
const MAX_QUEUE_PER_SESSION = 4;
const SUBC_HEADER_LEN = 17;
const SUBC_PROTOCOL_VERSION = 1;
const FRAME_REQUEST = 0;
const FRAME_RESPONSE = 1;
const FRAME_ERROR = 5;
const PRIORITY_BACKGROUND_FLAGS = 2 << 1;
const AUTH_CLIENT_DOMAIN = "subc-client-v1";
const AUTH_SERVER_DOMAIN = "subc-server-v1";
const NONCE_LEN = 32;
const PROOF_LEN = 32;
const CONNECT_BACKOFF_INITIAL_MS = 1_000;
const CONNECT_BACKOFF_MAX_MS = 30_000;
const REQUEST_TIMEOUT_MS = 5_000;
const HANDSHAKE_TIMEOUT_MS = 2_000;

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
        method: "shadow_reset" | "state_sync" | "shadow_transform";
        body: unknown;
    }): Promise<unknown>;
    closeSession?(sessionId: string): void;
}

export interface ShadowSender {
    enqueue(pass: ShadowTransformPass): void;
    resetSession(sessionId: string, reason: string): void;
    clearSession(sessionId: string): void;
    getStats(sessionId: string): Readonly<ShadowSenderCounters>;
}

interface ShadowSenderCounters {
    enqueued: number;
    dropped_oldest: number;
    send_failures: number;
    connection_skips: number;
    ordinal_unresolved: number;
    ordinal_mismatch: number;
    state_sync_rejects: number;
    generation_rejects: number;
    resets_sent: number;
    transforms_sent: number;
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
    requireResetReason: string | null;
    blockedUntilReset: boolean;
    seedPassPending: boolean;
    quarantineRetryAttempted: boolean;
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
        compartments: unknown[];
        memories: unknown[];
        memory_mutations: unknown[];
        workspace: ShadowWorkspacePayload | null;
        last_todo_state: string;
    };
    watermarks: ShadowWatermarks;
}

interface ConnectionInfo {
    schema: number;
    endpoints: Array<{ host: string; port: number }>;
    key: number[];
    daemon_id: number[];
    pid: number;
    daemon_ver: string;
}

interface SubcFrame {
    type: number;
    channel: number;
    corr: number;
    body: Buffer;
}

function emptyCounters(): ShadowSenderCounters {
    return {
        enqueued: 0,
        dropped_oldest: 0,
        send_failures: 0,
        connection_skips: 0,
        ordinal_unresolved: 0,
        ordinal_mismatch: 0,
        state_sync_rejects: 0,
        generation_rejects: 0,
        resets_sent: 0,
        transforms_sent: 0,
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
        requireResetReason: "cold_start",
        blockedUntilReset: false,
        seedPassPending: false,
        quarantineRetryAttempted: false,
        counters: emptyCounters(),
    };
}

function cloneJson<T>(value: T): T {
    return JSON.parse(JSON.stringify(value)) as T;
}

function stableHash(value: string): string {
    return createHmac("sha256", "magic-context-shadow-watermark").update(value).digest("hex");
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
    kind: "tag_prefix" | "ctx_search_hint";
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
    const tsOutput = cloneJson(args.outputMessages) as unknown[];
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
        const cloneMessage = tsOutput[msgIndex] as { parts?: unknown[] } | undefined;
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
            const cloneMessage = tsOutput[msgIndex] as { parts?: unknown[] } | undefined;
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

    return { ts_output: tsOutput, normalizations };
}

export function resolveOrdinalsForShadow(args: {
    sessionId: string;
    messages: MessageLike[];
    generation: number;
    memoGeneration: number;
    memo: Map<string, number>;
}):
    | { ok: true; annotatedInput: unknown[]; memoGeneration: number }
    | { ok: false; reason: "unresolved" | "mismatch"; messageId?: string } {
    const memo = args.memo;
    if (args.memoGeneration !== args.generation) {
        memo.clear();
    }

    const ordinalById = withRawSessionMessageCache(() => {
        const raw = readRawSessionMessages(args.sessionId);
        return new Map(raw.map((message) => [message.id, message.ordinal]));
    });

    // The transform captured this array before mutating the live messages. Annotate
    // that private snapshot directly instead of cloning the full history again.
    const annotated = args.messages as unknown as Array<Record<string, unknown>>;
    const resolved: Array<number | undefined> = new Array(annotated.length);
    let firstUnresolvedId: string | undefined;
    for (let index = 0; index < annotated.length; index += 1) {
        const messageId = getMessageId(args.messages[index]);
        if (!messageId) return { ok: false, reason: "unresolved" };
        let ordinal = ordinalById.get(messageId);
        if (ordinal === undefined) {
            // The active raw-message cache can be primed TAIL-ONLY (post-boundary)
            // by the transform on large sessions, so wire messages below the prime
            // floor never appear in it and would starve the shadow lane forever.
            // Fall back to a canonical by-id DB read — it computes the absolute
            // ordinal with the same ORDER BY/COUNT semantics as the full read, so
            // the memo drift check below keeps its exact meaning (a fresh value is
            // re-derived every pass; deleted messages stay "unresolved"). Below-
            // floor ids per pass are bounded by the marker lag (a handful), so the
            // extra point reads stay off the hot-path cost radar.
            ordinal = readRawSessionMessageById(args.sessionId, messageId)?.ordinal;
        }
        if (ordinal === undefined && firstUnresolvedId === undefined) {
            firstUnresolvedId = messageId;
        }
        resolved[index] = ordinal;
    }

    // OpenCode persists assistant rows at step/turn completion while the transform
    // runs mid-turn, so the newest wire message(s) routinely have no DB row yet.
    // Those live-tail messages are always a contiguous SUFFIX of the wire array in
    // canonical order, so their eventual ordinals are exactly "last persisted + n".
    // Assign those provisionally instead of skipping the pass — otherwise every
    // ACTIVE pass (the ones the byte-compare exists for) starves. If interleaved
    // persistence ever lands a different real ordinal, the memo drift check below
    // reports "mismatch" on the next pass and the caller's shadow_reset self-heals.
    let suffixStart = annotated.length;
    while (suffixStart > 0 && resolved[suffixStart - 1] === undefined) {
        suffixStart -= 1;
    }
    for (let index = 0; index < suffixStart; index += 1) {
        if (resolved[index] === undefined) {
            // A hole BEFORE a resolved message is not the live tail (deleted or
            // foreign id) — keep the fail-skip so we never fabricate history.
            return { ok: false, reason: "unresolved", messageId: firstUnresolvedId };
        }
    }
    if (suffixStart < annotated.length) {
        const base = suffixStart > 0 ? (resolved[suffixStart - 1] as number) : -1;
        for (let index = suffixStart; index < annotated.length; index += 1) {
            resolved[index] = base + (index - suffixStart) + 1;
        }
    }

    for (let index = 0; index < annotated.length; index += 1) {
        const messageId = getMessageId(args.messages[index]) as string;
        const ordinal = resolved[index] as number;
        const prior = memo.get(messageId);
        if (prior !== undefined && prior !== ordinal) {
            return { ok: false, reason: "mismatch", messageId };
        }
        memo.set(messageId, ordinal);
        annotated[index].absolute_ordinal = ordinal;
    }

    return { ok: true, annotatedInput: annotated, memoGeneration: args.generation };
}

function ordinalForMessageId(args: {
    rawById: ReadonlyMap<string, RawMessage>;
    messageId: string;
    generation: number;
    state: SessionQueueState;
}): number | null | "mismatch" {
    if (args.state.idOrdinalMemoGeneration !== args.generation) {
        args.state.idOrdinalMemo.clear();
        args.state.idOrdinalMemoGeneration = args.generation;
    }
    const found = args.rawById.get(args.messageId);
    if (!found) return null;
    const prior = args.state.idOrdinalMemo.get(args.messageId);
    if (prior !== undefined && prior !== found.ordinal) return "mismatch";
    args.state.idOrdinalMemo.set(args.messageId, found.ordinal);
    return found.ordinal;
}

function flatBlockCountForRawMessage(message: RawMessage | undefined): number {
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
    raw: RawMessage | undefined,
    edge: "start" | "end",
): string {
    const blockIndex = edge === "start" ? 0 : flatBlockCountForRawMessage(raw) - 1;
    return `${messageId}#${blockIndex}`;
}

export function resolveDeclaredTrimForShadow(args: {
    db: ContextDatabase;
    sessionId: string;
}): ShadowDeclaredTrim | null {
    const marker = getPersistedCompactionMarkerState(args.db, args.sessionId);
    if (!marker || marker.boundaryOrdinal < 1) return null;
    const targetEndMessageId = marker.targetEndMessageId ?? marker.boundaryMessageId;
    if (!targetEndMessageId) return null;
    const markerKey = `${marker.boundaryOrdinal}:${targetEndMessageId}`;
    const cached = declaredTrimBySession.get(args.sessionId);
    if (cached?.markerKey === markerKey) return cached.trim;

    const raw = withRawSessionMessageCache(() => readRawSessionMessages(args.sessionId));
    const rawById = new Map(raw.map((message) => [message.id, message]));
    const boundaryRaw = rawById.get(targetEndMessageId);
    if (!boundaryRaw) return null;
    const compartments = getCompartmentsByEndMessageId(args.db, args.sessionId, targetEndMessageId);
    const boundaryCompartment = compartments.find(
        (compartment) => compartment.endMessage === marker.boundaryOrdinal,
    );
    if (!boundaryCompartment) return null;
    const trim: ShadowDeclaredTrim = {
        flat_boundary_id: flatBlockIdForRawMessage(targetEndMessageId, boundaryRaw, "end"),
        boundary_bare_message_id: targetEndMessageId,
        boundary_absolute_ordinal: marker.boundaryOrdinal,
        next_absolute_ordinal: marker.boundaryOrdinal + 1,
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

function resolveShadowWorkspaceContext(
    db: ContextDatabase,
    projectPath?: string,
): ShadowWorkspaceContext {
    if (!projectPath) {
        return {
            workspace: null,
            expandedIdentities: [],
            ownIdentities: [],
            shareCategories: null,
        };
    }
    const identitySet = resolveWorkspaceIdentitySet(db, projectPath);
    if (identitySet.identities.length <= 1) {
        return {
            workspace: null,
            expandedIdentities: [projectPath],
            ownIdentities: [projectPath],
            shareCategories: null,
        };
    }
    const expanded = expandWorkspaceIdentitySetWithAliases(db, identitySet.identities);
    const ownIdentities = expanded.expandedIdentities.filter(
        (identity) => expanded.canonicalIdentityByStoredPath.get(identity) === projectPath,
    );
    if (ownIdentities.length === 0) ownIdentities.push(projectPath);
    const shareCategories = resolveWorkspaceShareCategories(db, projectPath) ?? [];
    const members = [
        projectPath,
        ...expanded.expandedIdentities
            .filter((identity) => identity !== projectPath)
            .sort((left, right) => left.localeCompare(right)),
    ];
    return {
        workspace: {
            fingerprint: computeWorkspaceEpochFingerprint(db, identitySet.identities),
            members: members.map((member) => ({
                project_path: member,
                share_categories: [...shareCategories],
            })),
        },
        expandedIdentities: members,
        ownIdentities,
        shareCategories,
    };
}

function loadWatermarks(args: {
    db: ContextDatabase;
    sessionId: string;
    projectPath?: string;
    workspace: ShadowWorkspaceContext;
}): ShadowWatermarks {
    const sessionMeta = getOrCreateSessionMeta(args.db, args.sessionId);
    const compartmentRow = args.db
        .prepare(
            "SELECT COALESCE(MAX(sequence), -1) AS max_sequence FROM compartments WHERE session_id = ?",
        )
        .get(args.sessionId) as { max_sequence?: number } | undefined;
    const memoryId = args.projectPath
        ? getMaxMemoryIdForProjects(
              args.db,
              args.workspace.expandedIdentities,
              args.workspace.ownIdentities,
              args.workspace.shareCategories,
          )
        : 0;
    const m0Row = args.db
        .prepare("SELECT COALESCE(MAX(id), 0) AS max_id FROM m0_mutation_log WHERE session_id = ?")
        .get(args.sessionId) as { max_id?: number } | undefined;
    const memoryMutationId = args.projectPath
        ? (getMaxMemoryMutationIdForProjects(args.db, args.workspace.expandedIdentities) ?? 0)
        : 0;
    return {
        compartment_sequence: compartmentRow?.max_sequence ?? -1,
        memory_id: memoryId,
        m0_mutation_id: m0Row?.max_id ?? 0,
        memory_mutation_id: memoryMutationId,
        last_todo_state_hash: stableHash(sessionMeta.lastTodoState ?? ""),
    };
}

function watermarksEqual(left: ShadowWatermarks | null, right: ShadowWatermarks): boolean {
    return (
        left !== null &&
        left.compartment_sequence === right.compartment_sequence &&
        left.memory_id === right.memory_id &&
        left.m0_mutation_id === right.m0_mutation_id &&
        left.memory_mutation_id === right.memory_mutation_id &&
        left.last_todo_state_hash === right.last_todo_state_hash
    );
}

function serializeCompartment(args: {
    sessionId: string;
    compartment: ReturnType<typeof getCompartments>[number];
    rawById: Map<string, RawMessage>;
    state: SessionQueueState;
}): unknown | null | "mismatch" {
    const startOrdinal = ordinalForMessageId({
        rawById: args.rawById,
        messageId: args.compartment.startMessageId,
        generation: args.state.shadowGeneration,
        state: args.state,
    });
    const endOrdinal = ordinalForMessageId({
        rawById: args.rawById,
        messageId: args.compartment.endMessageId,
        generation: args.state.shadowGeneration,
        state: args.state,
    });
    if (startOrdinal === "mismatch" || endOrdinal === "mismatch") return "mismatch";
    if (startOrdinal === null || endOrdinal === null) return null;
    const startCreatedAt = args.rawById.get(args.compartment.startMessageId)?.createdAt;
    const endCreatedAt = args.rawById.get(args.compartment.endMessageId)?.createdAt;
    const dateRange =
        typeof startCreatedAt === "number" && typeof endCreatedAt === "number"
            ? {
                  start_date: formatDate(startCreatedAt),
                  end_date: formatDate(endCreatedAt),
              }
            : {};
    return {
        sequence: args.compartment.sequence,
        start_message: startOrdinal,
        end_message: endOrdinal,
        start_message_id: flatBlockIdForRawMessage(
            args.compartment.startMessageId,
            args.rawById.get(args.compartment.startMessageId),
            "start",
        ),
        end_message_id: flatBlockIdForRawMessage(
            args.compartment.endMessageId,
            args.rawById.get(args.compartment.endMessageId),
            "end",
        ),
        ...dateRange,
        title: args.compartment.title,
        content: args.compartment.content,
        p1: args.compartment.p1,
        p2: args.compartment.p2,
        p3: args.compartment.p3,
        p4: args.compartment.p4,
        importance: args.compartment.importance,
        episode_type: args.compartment.episodeType,
        legacy: args.compartment.legacy,
        created_at: args.compartment.createdAt,
    };
}

function buildStateSyncPayload(args: {
    state: SessionQueueState;
    pass: Pick<ShadowTransformPass, "db" | "sessionId" | "projectPath" | "passInputs">;
    force: boolean;
}): ShadowStateSyncPayload | null | "m0_mutation" | "mismatch" | "unresolved" {
    const workspace = resolveShadowWorkspaceContext(args.pass.db, args.pass.projectPath);
    const currentWatermarks = loadWatermarks({
        db: args.pass.db,
        sessionId: args.pass.sessionId,
        projectPath: args.pass.projectPath,
        workspace,
    });
    if (
        !args.force &&
        args.state.lastAckedWatermarks &&
        currentWatermarks.m0_mutation_id > args.state.lastAckedWatermarks.m0_mutation_id
    ) {
        return "m0_mutation";
    }
    if (!args.force && watermarksEqual(args.state.lastAckedWatermarks, currentWatermarks)) {
        return null;
    }

    const acked = args.force
        ? {
              compartment_sequence: -1,
              memory_id: 0,
              m0_mutation_id: 0,
              memory_mutation_id: 0,
              last_todo_state_hash: "",
          }
        : (args.state.lastAckedWatermarks ?? {
              compartment_sequence: -1,
              memory_id: 0,
              m0_mutation_id: 0,
              memory_mutation_id: 0,
              last_todo_state_hash: "",
          });
    const rawById = withRawSessionMessageCache(
        () =>
            new Map(
                readRawSessionMessages(args.pass.sessionId).map((message) => [message.id, message]),
            ),
    );
    const compartments: unknown[] = [];
    for (const compartment of getCompartments(args.pass.db, args.pass.sessionId)) {
        if (compartment.sequence <= acked.compartment_sequence) continue;
        const serialized = serializeCompartment({
            sessionId: args.pass.sessionId,
            compartment,
            rawById,
            state: args.state,
        });
        if (serialized === "mismatch") return "mismatch";
        if (serialized === null) return "unresolved";
        compartments.push(serialized);
    }

    const allMemories = args.pass.projectPath
        ? workspace.workspace
            ? getMemoriesByProjects(
                  args.pass.db,
                  workspace.expandedIdentities,
                  ["active", "permanent"],
                  args.pass.passInputs.now_ms,
                  workspace.ownIdentities,
                  workspace.shareCategories,
              )
            : getMemoriesByProject(
                  args.pass.db,
                  args.pass.projectPath,
                  ["active", "permanent"],
                  args.pass.passInputs.now_ms,
              )
        : [];
    const memories = allMemories
        .filter((memory) => memory.id > acked.memory_id)
        .map((memory) => ({
            id: memory.id,
            project_path: memory.projectPath,
            category: memory.category,
            content: memory.content,
            normalized_hash: memory.normalizedHash,
            importance: memory.importance,
            scope: memory.scope,
            shareable: memory.shareable,
            source_session_id: memory.sourceSessionId,
            source_type: memory.sourceType,
            seen_count: memory.seenCount,
            retrieval_count: memory.retrievalCount,
            first_seen_at: memory.firstSeenAt,
            created_at: memory.createdAt,
            updated_at: memory.updatedAt,
            last_seen_at: memory.lastSeenAt,
            last_retrieved_at: memory.lastRetrievedAt,
            status: memory.status,
            expires_at: memory.expiresAt,
            verification_status: memory.verificationStatus,
            verified_at: memory.verifiedAt,
            superseded_by_memory_id: memory.supersededByMemoryId,
            merged_from: memory.mergedFrom,
            metadata_json: memory.metadataJson,
        }));

    const renderedMemoryIds = allMemories.map((memory) => memory.id);
    const memoryMutations = args.pass.projectPath
        ? getMemoryMutationsForRenderByProjects(
              args.pass.db,
              workspace.expandedIdentities,
              acked.memory_mutation_id,
              renderedMemoryIds,
          ).map((row) => ({
              id: row.id,
              project_path: row.projectPath,
              mutation_type: row.mutationType,
              target_memory_id: row.targetMemoryId,
              superseded_by_id: row.supersededById,
              category: row.category,
              new_content: row.newContent,
              queued_at: row.queuedAt,
          }))
        : [];
    const sessionMeta = getOrCreateSessionMeta(args.pass.db, args.pass.sessionId);

    return {
        method: "state_sync",
        params: {
            shadow_generation: args.state.shadowGeneration,
            expected_shadow_seq: args.state.lastAckedSeq,
            compartments,
            memories,
            memory_mutations: memoryMutations,
            workspace: workspace.workspace,
            last_todo_state: sessionMeta.lastTodoState ?? "",
        },
        watermarks: currentWatermarks,
    } satisfies ShadowStateSyncPayload;
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
    if (isRecord(error) && typeof error.code === "string") return error.code;
    return null;
}

function isPeerReject(error: unknown): boolean {
    const code = errorCode(error);
    if (
        code === "stale_generation" ||
        code === "shadow_generation_mismatch" ||
        code === "shadow_seq_mismatch" ||
        code === "seq_mismatch" ||
        code === "cas_mismatch" ||
        code === "state_cas_reject"
    ) {
        return true;
    }
    const text = error instanceof Error ? error.message : String(error);
    return (
        text.includes("generation") ||
        text.includes("shadow_seq") ||
        text.includes("seq mismatch") ||
        text.includes("CAS")
    );
}

function isConnectionFailure(error: unknown): boolean {
    const code = errorCode(error);
    if (code === "ENOENT" || code === "ECONNREFUSED" || code === "ECONNRESET") return true;
    const text = error instanceof Error ? error.message : String(error);
    return text.includes("backoff") || text.includes("connection") || text.includes("ECONN");
}

/**
 * Flatten a `{method, params}` payload into the wire shape the module expects.
 * The Rust handlers deserialize the WHOLE request value (serde_json::from_value
 * on ShadowStateSyncWire / ShadowTransformWire / ShadowResetWire), so op fields
 * must live at the top level beside `method` — a nested `params` object never
 * reaches the parser and hard-required fields like `shadow_generation` reject
 * with invalid_params. Builders keep the typed `{method, params}` shape for
 * testability; this is the single serialization point.
 */
function toFlatWireBody(payload: { method: string; params: Record<string, unknown> }): unknown {
    return { method: payload.method, ...payload.params };
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

export function createShadowSender(options: { transport?: ShadowTransport } = {}): ShadowSender {
    const transport = options.transport ?? new SubcShadowTransport();
    const sessions = new Map<string, SessionQueueState>();

    const getState = (sessionId: string): SessionQueueState => {
        let state = sessions.get(sessionId);
        if (!state) {
            state = createSessionQueueState();
            sessions.set(sessionId, state);
        }
        return state;
    };

    const schedule = (sessionId: string): void => {
        const state = getState(sessionId);
        if (state.running) return;
        state.running = true;
        void runQueue(sessionId, state).finally(() => {
            state.running = false;
            if (state.queue.length > 0) schedule(sessionId);
        });
    };

    const pushWork = (sessionId: string, work: ShadowWorkItem): void => {
        const state = getState(sessionId);
        if (work.kind === "pass") state.counters.enqueued += 1;
        if (
            work.kind === "pass" &&
            state.queue.filter((item) => item.kind === "pass").length >= MAX_QUEUE_PER_SESSION
        ) {
            const oldestIndex = state.queue.findIndex((item) => item.kind === "pass");
            if (oldestIndex >= 0) {
                state.queue.splice(oldestIndex, 1);
                state.counters.dropped_oldest += 1;
                sessionLog(sessionId, "shadow: dropped oldest queued pass (cap=4)");
            }
        }
        state.queue.push(work);
        schedule(sessionId);
    };

    const runQueue = async (sessionId: string, state: SessionQueueState): Promise<void> => {
        while (state.queue.length > 0) {
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
                } catch (error) {
                    state.counters.send_failures += 1;
                    state.initialized = false;
                    state.blockedUntilReset = true;
                    state.requireResetReason ??= item.reason || "reset_retry";
                    sessionLog(sessionId, "shadow: reset failed (ignored):", error);
                }
                continue;
            }
            try {
                await processPass(state, item.pass);
            } catch (error) {
                state.counters.send_failures += 1;
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
                }
                sessionLog(sessionId, "shadow: send failed (ignored):", error);
            }
        }
    };

    const performReset = async (args: {
        sessionId: string;
        state: SessionQueueState;
        reason: string;
        projectRoot?: string;
    }): Promise<void> => {
        const projectRoot = args.projectRoot ?? process.cwd();
        const body = toFlatWireBody(buildShadowResetBody(args));
        const response = await transport.call({
            sessionId: args.sessionId,
            projectRoot,
            method: "shadow_reset",
            body,
        });
        args.state.shadowGeneration = numericAck(
            response,
            ["shadow_generation", "generation"],
            args.state.shadowGeneration + 1,
        );
        args.state.lastAckedSeq = numericAck(response, ["shadow_seq", "seq"], 0);
        args.state.lastAckedWatermarks = null;
        args.state.idOrdinalMemo.clear();
        args.state.idOrdinalMemoGeneration = args.state.shadowGeneration;
        clearDeclaredTrimForSession(args.sessionId);
        args.state.initialized = true;
        args.state.blockedUntilReset = false;
        args.state.requireResetReason = null;
        // Every fresh lineage must commit one normal transform before byte comparison.
        // The complete sync makes source state available; the seed pass then establishes
        // the shadow lane's own first-render cache and boundary state.
        args.state.seedPassPending = true;
        args.state.counters.resets_sent += 1;
        sessionLog(
            args.sessionId,
            `shadow: reset acknowledged (generation=${args.state.shadowGeneration})`,
        );
    };

    const processPass = async (
        state: SessionQueueState,
        pass: ShadowTransformPass,
    ): Promise<void> => {
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
        if (state.blockedUntilReset) return;

        let resolved: ReturnType<typeof resolveOrdinalsForShadow>;
        try {
            resolved = resolveOrdinalsForShadow({
                sessionId: pass.sessionId,
                messages: pass.inputMessages,
                generation: state.shadowGeneration,
                memoGeneration: state.idOrdinalMemoGeneration,
                memo: state.idOrdinalMemo,
            });
        } catch (error) {
            sessionLog(pass.sessionId, "shadow: capture failed (ignored):", error);
            return;
        }
        if (!resolved.ok) {
            if (resolved.reason === "mismatch") {
                state.counters.ordinal_mismatch += 1;
                state.requireResetReason = "ordinal_mismatch";
                state.blockedUntilReset = true;
                state.idOrdinalMemo.clear();
                await performReset({
                    sessionId: pass.sessionId,
                    state,
                    reason: "ordinal_mismatch",
                    projectRoot: pass.projectRoot,
                });
            } else {
                state.counters.ordinal_unresolved += 1;
                sessionLog(
                    pass.sessionId,
                    `shadow: pass skipped; unresolved ordinal for ${resolved.messageId ?? "unknown"}`,
                );
            }
            return;
        }
        state.idOrdinalMemoGeneration = resolved.memoGeneration;

        let preparedPass: PreparedShadowPass;
        let syncPayload: ReturnType<typeof buildStateSyncPayload>;
        try {
            const declaredTrim = resolveDeclaredTrimForShadow({
                db: pass.db,
                sessionId: pass.sessionId,
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
                shadowNormalizations: denormalized.normalizations,
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
            syncPayload = buildStateSyncPayload({
                state,
                pass: preparedPass,
                force: state.lastAckedWatermarks === null,
            });
        } catch (error) {
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
                const fullSync = buildStateSyncPayload({ state, pass: preparedPass, force: true });
                if (fullSync === "m0_mutation") {
                    throw new Error("forced state sync unexpectedly requested another m0 reset");
                }
                syncPayload = fullSync;
            } catch (error) {
                sessionLog(pass.sessionId, "shadow: capture failed (ignored):", error);
                return;
            }
        }
        if (syncPayload === "mismatch") {
            state.counters.ordinal_mismatch += 1;
            state.requireResetReason = "ordinal_mismatch";
            await performReset({
                sessionId: pass.sessionId,
                state,
                reason: "ordinal_mismatch",
                projectRoot: pass.projectRoot,
            });
            return;
        }
        if (syncPayload === "unresolved") {
            state.counters.ordinal_unresolved += 1;
            sessionLog(
                pass.sessionId,
                "shadow: state sync skipped; compartment ordinal unresolved",
            );
            return;
        }
        if (syncPayload !== null) {
            const response = await transport.call({
                sessionId: pass.sessionId,
                projectRoot: pass.projectRoot,
                method: "state_sync",
                body: toFlatWireBody(syncPayload),
            });
            state.lastAckedSeq = numericAck(
                response,
                ["shadow_seq", "seq"],
                syncPayload.params.expected_shadow_seq + 1,
            );
            state.lastAckedWatermarks = syncPayload.watermarks;
        }

        const transformBody = toFlatWireBody(
            buildShadowTransformBody({ pass: preparedPass, state }),
        );
        const response = await transport.call({
            sessionId: pass.sessionId,
            projectRoot: pass.projectRoot,
            method: "shadow_transform",
            body: transformBody,
        });
        const ack = extractAckValue(response);
        state.seedPassPending = false;
        state.counters.transforms_sent += 1;
        if (ack.divergence || ack.divergence_class || ack.hard_divergence) {
            sessionLog(pass.sessionId, "shadow: divergence report", ack);
        }
        if (ack.quarantined === true && !state.quarantineRetryAttempted) {
            state.quarantineRetryAttempted = true;
            await performReset({
                sessionId: pass.sessionId,
                state,
                reason: "quarantine_reseed",
                projectRoot: pass.projectRoot,
            });
            await processPass(state, pass);
        }
    };

    return {
        enqueue(pass: ShadowTransformPass): void {
            pushWork(pass.sessionId, { kind: "pass", pass });
        },
        resetSession(sessionId: string, reason: string): void {
            const state = getState(sessionId);
            state.queue.length = 0;
            state.requireResetReason = reason;
            state.blockedUntilReset = true;
            transport.closeSession?.(sessionId);
            pushWork(sessionId, { kind: "reset", sessionId, reason });
        },
        clearSession(sessionId: string): void {
            sessions.delete(sessionId);
            clearDeclaredTrimForSession(sessionId);
            transport.closeSession?.(sessionId);
        },
        getStats(sessionId: string): Readonly<ShadowSenderCounters> {
            return { ...getState(sessionId).counters };
        },
    };
}

class SubcShadowTransport implements ShadowTransport {
    private connectionFile: string;
    private moduleId: string;
    private socket: net.Socket | null = null;
    private reader: SocketReader | null = null;
    private nextCorr = 1;
    private routes = new Map<string, number>();
    private pending = Promise.resolve();
    private nextProbeMs = 0;
    private backoffMs = CONNECT_BACKOFF_INITIAL_MS;
    private requestTimeoutMs: number;

    constructor(
        connectionFile?: string,
        moduleId = DEFAULT_MODULE_ID,
        requestTimeoutMs = REQUEST_TIMEOUT_MS,
    ) {
        this.connectionFile = connectionFile ?? getDefaultConnectionFile();
        this.moduleId = moduleId;
        this.requestTimeoutMs = requestTimeoutMs;
    }

    async call(args: {
        sessionId: string;
        projectRoot: string;
        method: "shadow_reset" | "state_sync" | "shadow_transform";
        body: unknown;
    }): Promise<unknown> {
        const run = async (): Promise<unknown> => {
            const route = await this.ensureRoute(args.sessionId, args.projectRoot);
            return await this.unaryJson(route, args.body);
        };
        const next = this.pending.then(run, run);
        this.pending = next.then(
            () => undefined,
            () => undefined,
        );
        return next;
    }

    closeSession(sessionId: string): void {
        this.routes.delete(sessionId);
    }

    private async ensureRoute(sessionId: string, projectRoot: string): Promise<number> {
        const existing = this.routes.get(sessionId);
        if (existing !== undefined) return existing;
        await this.ensureConnected();
        const body: Record<string, unknown> = {
            op: "route.open",
            // mc-module registers a ToolProvider role in its manifest (see
            // crates/mc-module/src/lib.rs manifest()); it does NOT provide a
            // management surface. Opening with the wrong kind makes the daemon
            // reject every route with target_unavailable.
            target: { kind: "tool_provider", module_id: this.moduleId },
            identity: {
                project_root: projectRoot,
                harness: getHarness(),
                session: `shadow:${sessionId}`,
            },
        };
        const moduleId = process.env.SUBC_MODULE_ID;
        const launchNonce = process.env.SUBC_LAUNCH_NONCE;
        if (moduleId && launchNonce) {
            body.consumer_identity = { module_id: moduleId, launch_nonce: launchNonce };
        }
        const response = await this.unaryJson(0, body);
        const ack = extractAckValue(response);
        const route = ack.route_channel;
        if (typeof route !== "number")
            throw new Error("connection route.open missing route_channel");
        this.routes.set(sessionId, route);
        return route;
    }

    private async ensureConnected(): Promise<void> {
        if (this.socket && !this.socket.destroyed && this.reader) return;
        const now = Date.now();
        if (now < this.nextProbeMs) {
            throw new Error(`connection backoff active until ${this.nextProbeMs}`);
        }
        let candidate: net.Socket | null = null;
        try {
            const conn = readConnectionInfo(this.connectionFile);
            const endpoint = conn.endpoints[0];
            if (!endpoint) throw new Error("connection file has no endpoint");
            candidate = await connectTcp(endpoint.host, endpoint.port, HANDSHAKE_TIMEOUT_MS);
            const reader = new SocketReader(candidate);
            await authenticateSubcClient(candidate, reader, conn, HANDSHAKE_TIMEOUT_MS);
            const socket = candidate;
            this.socket = socket;
            this.reader = reader;
            this.routes.clear();
            this.backoffMs = CONNECT_BACKOFF_INITIAL_MS;
            this.nextProbeMs = 0;
            socket.once("close", () => {
                if (this.socket === socket) this.invalidateConnection(socket);
            });
        } catch (error) {
            candidate?.destroy();
            this.invalidateConnection();
            this.nextProbeMs = Date.now() + this.backoffMs;
            this.backoffMs = Math.min(this.backoffMs * 2, CONNECT_BACKOFF_MAX_MS);
            throw error;
        }
    }

    private invalidateConnection(socket: net.Socket | null = this.socket): void {
        if (socket && this.socket && socket !== this.socket) return;
        this.socket = null;
        this.reader = null;
        this.routes.clear();
        socket?.destroy();
    }

    private async unaryJson(channel: number, body: unknown): Promise<unknown> {
        await this.ensureConnected();
        const socket = this.socket;
        const reader = this.reader;
        if (!socket || !reader) throw new Error("connection unavailable");
        const corr = this.nextCorr++;
        try {
            await writeFrame(socket, {
                type: FRAME_REQUEST,
                channel,
                corr,
                body: Buffer.from(JSON.stringify(body)),
            });
            const frame = await readTerminalFor(reader, channel, corr, this.requestTimeoutMs);
            if (frame.type === FRAME_ERROR) {
                throw parseErrorBody(frame.body);
            }
            if (frame.type !== FRAME_RESPONSE) {
                throw new Error(`unexpected subc frame type ${frame.type}`);
            }
            if (frame.body.length === 0) return null;
            return JSON.parse(frame.body.toString("utf8"));
        } catch (error) {
            // A timeout or malformed/error frame can leave unread bytes buffered.
            // Reusing that stream would make the next response start mid-frame.
            this.invalidateConnection(socket);
            throw error;
        }
    }
}

function readConnectionInfo(path: string): ConnectionInfo {
    if (!existsSync(path)) throw new Error(`connection file not found: ${path}`);
    try {
        const mode = statSync(path).mode & 0o777;
        if ((mode & 0o077) !== 0)
            throw new Error(`connection file has insecure mode ${mode.toString(8)}`);
    } catch (error) {
        if (error instanceof Error && error.message.includes("insecure")) throw error;
    }
    const parsed = JSON.parse(readFileSync(path, "utf8")) as ConnectionInfo;
    if (parsed.schema !== 1) throw new Error(`unsupported connection schema ${parsed.schema}`);
    if (!Array.isArray(parsed.endpoints) || parsed.endpoints.length === 0) {
        throw new Error("connection file has no endpoints");
    }
    if (!Array.isArray(parsed.key) || parsed.key.length < 32)
        throw new Error("connection key too short");
    return parsed;
}

function connectTcp(host: string, port: number, timeoutMs: number): Promise<net.Socket> {
    return new Promise((resolve, reject) => {
        const socket = net.createConnection({ host, port });
        const timer = setTimeout(() => {
            socket.destroy();
            reject(new Error("connection timeout"));
        }, timeoutMs);
        socket.once("connect", () => {
            clearTimeout(timer);
            resolve(socket);
        });
        socket.once("error", (error) => {
            clearTimeout(timer);
            reject(error);
        });
    });
}

class SocketReader {
    private chunks: Buffer[] = [];
    private buffered = 0;
    private waiters: Array<() => void> = [];
    private closed = false;

    constructor(socket: net.Socket) {
        socket.on("data", (chunk) => {
            this.chunks.push(Buffer.from(chunk));
            this.buffered += chunk.length;
            this.flushWaiters();
        });
        socket.on("close", () => {
            this.closed = true;
            this.flushWaiters();
        });
        socket.on("error", () => {
            this.closed = true;
            this.flushWaiters();
        });
    }

    async readExact(length: number, timeoutMs: number): Promise<Buffer> {
        const deadline = Date.now() + timeoutMs;
        while (this.buffered < length) {
            if (this.closed) throw new Error("connection closed");
            const remaining = deadline - Date.now();
            if (remaining <= 0) throw new Error("read timeout");
            await new Promise<void>((resolve, reject) => {
                const timer = setTimeout(() => {
                    this.waiters = this.waiters.filter((waiter) => waiter !== onReady);
                    reject(new Error("read timeout"));
                }, remaining);
                const onReady = () => {
                    clearTimeout(timer);
                    resolve();
                };
                this.waiters.push(onReady);
            });
        }
        const out = Buffer.allocUnsafe(length);
        let offset = 0;
        while (offset < length) {
            const first = this.chunks[0];
            const take = Math.min(first.length, length - offset);
            first.copy(out, offset, 0, take);
            offset += take;
            if (take === first.length) this.chunks.shift();
            else this.chunks[0] = first.subarray(take);
            this.buffered -= take;
        }
        return out;
    }

    private flushWaiters(): void {
        const waiters = this.waiters.splice(0);
        for (const waiter of waiters) waiter();
    }
}

async function writeAuthMessage(socket: net.Socket, value: unknown): Promise<void> {
    const json = Buffer.from(JSON.stringify(value));
    const len = Buffer.allocUnsafe(4);
    len.writeUInt32LE(json.length, 0);
    socket.write(Buffer.concat([len, json]));
}

async function readAuthMessage(reader: SocketReader, timeoutMs: number): Promise<unknown> {
    const lenBytes = await reader.readExact(4, timeoutMs);
    const len = lenBytes.readUInt32LE(0);
    const body = await reader.readExact(len, timeoutMs);
    return JSON.parse(body.toString("utf8"));
}

function proof(
    key: Buffer,
    domain: string,
    clientNonce: Buffer,
    serverNonce: Buffer,
    daemonId: Buffer,
): Buffer {
    return createHmac("sha256", key)
        .update(domain)
        .update(clientNonce)
        .update(serverNonce)
        .update(daemonId)
        .digest();
}

async function authenticateSubcClient(
    socket: net.Socket,
    reader: SocketReader,
    conn: ConnectionInfo,
    timeoutMs: number,
): Promise<void> {
    const key = Buffer.from(conn.key);
    const clientNonce = randomBytes(NONCE_LEN);
    await writeAuthMessage(socket, { client_nonce: [...clientNonce], role: "client" });
    const serverProof = (await readAuthMessage(reader, timeoutMs)) as Record<string, unknown>;
    const serverNonce = Buffer.from((serverProof.server_nonce as number[]) ?? []);
    const daemonId = Buffer.from((serverProof.daemon_id as number[]) ?? []);
    const serverProofBytes = Buffer.from((serverProof.server_proof as number[]) ?? []);
    const expected = proof(key, AUTH_SERVER_DOMAIN, clientNonce, serverNonce, daemonId);
    if (
        daemonId.length !== conn.daemon_id.length ||
        !timingSafeEqual(daemonId, Buffer.from(conn.daemon_id)) ||
        serverProofBytes.length !== PROOF_LEN ||
        !timingSafeEqual(serverProofBytes, expected)
    ) {
        throw new Error("invalid subc server proof");
    }
    const clientAuth = proof(key, AUTH_CLIENT_DOMAIN, clientNonce, serverNonce, daemonId);
    await writeAuthMessage(socket, { client_auth: [...clientAuth] });
}

async function writeFrame(socket: net.Socket, frame: SubcFrame): Promise<void> {
    const header = Buffer.allocUnsafe(SUBC_HEADER_LEN);
    header.writeUInt32LE(frame.body.length, 0);
    header.writeUInt8(SUBC_PROTOCOL_VERSION, 4);
    header.writeUInt8(frame.type, 5);
    header.writeUInt8(PRIORITY_BACKGROUND_FLAGS, 6);
    header.writeUInt16LE(frame.channel, 7);
    header.writeBigUInt64LE(BigInt(frame.corr), 9);
    socket.write(Buffer.concat([header, frame.body]));
}

async function readFrame(reader: SocketReader, timeoutMs: number): Promise<SubcFrame> {
    const header = await reader.readExact(SUBC_HEADER_LEN, timeoutMs);
    const len = header.readUInt32LE(0);
    const version = header.readUInt8(4);
    if (version !== SUBC_PROTOCOL_VERSION)
        throw new Error(`unsupported subc frame version ${version}`);
    const type = header.readUInt8(5);
    const channel = header.readUInt16LE(7);
    const corr = Number(header.readBigUInt64LE(9));
    const body = len > 0 ? await reader.readExact(len, timeoutMs) : Buffer.alloc(0);
    return { type, channel, corr, body };
}

async function readTerminalFor(
    reader: SocketReader,
    channel: number,
    corr: number,
    timeoutMs: number,
): Promise<SubcFrame> {
    const deadline = Date.now() + timeoutMs;
    for (;;) {
        const remaining = deadline - Date.now();
        if (remaining <= 0) throw new Error("subc request timeout");
        const frame = await readFrame(reader, remaining);
        if (frame.channel === channel && frame.corr === corr) return frame;
    }
}

function parseErrorBody(body: Buffer): Error & { code?: string } {
    try {
        const parsed = JSON.parse(body.toString("utf8")) as { code?: string; message?: string };
        const error = new Error(
            `${parsed.code ?? "subc_error"}: ${parsed.message ?? "unknown"}`,
        ) as Error & { code?: string };
        if (typeof parsed.code === "string") error.code = parsed.code;
        return error;
    } catch {
        return new Error(body.toString("utf8") || "subc_error") as Error & { code?: string };
    }
}

export const __shadowSenderTest = {
    SocketReader,
    SubcShadowTransport,
    buildShadowResetBody,
    buildShadowTransformBody,
    buildStateSyncPayload,
    createSessionQueueState,
    denormalizeShadowOutput,
    flatBlockIdForRawMessage,
    resolveDeclaredTrimForShadow,
    resolveOrdinalsForShadow,
    toFlatWireBody,
};
