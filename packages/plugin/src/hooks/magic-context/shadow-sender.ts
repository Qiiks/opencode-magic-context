import { createHmac } from "node:crypto";
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
    getRawSessionStoredMessageCount,
    readRawSessionMessageOrdinalPage,
    readRawSessionMessagePartsById,
} from "./read-session-chunk";
import {
    isRawCompactionSummaryInfo,
    type RawMessageOrdinalAnchor,
    type RawMessageParts,
} from "./read-session-raw";
import type { MessageLike, TagNormalizationTarget } from "./tag-messages";
import { formatDate } from "./temporal-awareness";

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
const SHADOW_ORDINAL_PAGE_SIZE = 500;
const SHADOW_SEED_YIELD_EVERY_COMPARTMENTS = 10;
const SHADOW_SEED_BUDGET_MS = 30_000;

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
        method: "shadow_reset" | "state_sync" | "shadow_transform";
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
        seed_boundary_id: string | null;
        compartments: unknown[];
        memories: unknown[];
        memory_mutations: unknown[];
        workspace: ShadowWorkspacePayload | null;
        last_todo_state: string;
    };
    watermarks: ShadowWatermarks;
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
}): Promise<
    | {
          ok: true;
          annotatedInput: unknown[];
          memoGeneration: number;
          memoAnchor: RawMessageOrdinalAnchor | null;
          memoStoredCount: number;
          memoCanonicalCount: number;
          normalizations: ShadowNormalizationRecord[];
      }
    | { ok: false; reason: "unresolved" | "mismatch"; messageId?: string }
> {
    const memo = args.memo;
    const generationChanged = args.memoGeneration !== args.generation;
    if (generationChanged) memo.clear();

    let anchor = generationChanged ? null : (args.memoAnchor ?? null);
    let storedCount = generationChanged ? null : (args.memoStoredCount ?? null);
    let canonicalCount = generationChanged ? 0 : (args.memoCanonicalCount ?? 0);
    const priming = storedCount === null;
    if (priming) {
        memo.clear();
        anchor = null;
        canonicalCount = 0;
    }

    const newEntries: Array<ReturnType<typeof readRawSessionMessageOrdinalPage>[number]> = [];
    let pageAnchor = anchor;
    while (true) {
        const page = readRawSessionMessageOrdinalPage(
            args.sessionId,
            pageAnchor,
            SHADOW_ORDINAL_PAGE_SIZE,
        );
        if (page.length === 0) break;
        newEntries.push(...page);
        const last = page[page.length - 1];
        pageAnchor = { timeCreated: last.timeCreated, id: last.id };
        if (page.length < SHADOW_ORDINAL_PAGE_SIZE) break;
        await yieldToEventLoop();
    }

    const currentStoredCount = getRawSessionStoredMessageCount(args.sessionId);
    const expectedStoredCount = (storedCount ?? 0) + newEntries.length;
    if (currentStoredCount !== expectedStoredCount) {
        memo.clear();
        return { ok: false, reason: "mismatch" };
    }

    for (const entry of newEntries) {
        if (!entry.contributesOrdinal) continue;
        canonicalCount += 1;
        if (!entry.hasValidInfo) continue;
        const prior = memo.get(entry.id);
        if (prior !== undefined && prior !== canonicalCount) {
            memo.clear();
            return { ok: false, reason: "mismatch", messageId: entry.id };
        }
        memo.set(entry.id, canonicalCount);
    }
    anchor = pageAnchor;
    storedCount = currentStoredCount;

    const normalizations: ShadowNormalizationRecord[] = [];
    const visibleMessages = args.messages.filter((message) => {
        if (!isRawCompactionSummaryInfo(message.info)) return true;
        normalizations.push({
            kind: "summary_message",
            message_id: getMessageId(message),
            part_index: -1,
            field: "input",
            removed: JSON.stringify(message),
        });
        return false;
    });

    // The transform captured this array before mutating the live messages. Annotate
    // that private snapshot directly instead of cloning the full history again.
    const annotated = visibleMessages as unknown as Array<Record<string, unknown>>;
    const resolved: Array<number | undefined> = new Array(annotated.length);
    let firstUnresolvedId: string | undefined;
    for (let index = 0; index < annotated.length; index += 1) {
        const messageId = getMessageId(visibleMessages[index]);
        if (!messageId) return { ok: false, reason: "unresolved" };
        const ordinal = memo.get(messageId);
        if (ordinal === undefined && firstUnresolvedId === undefined) firstUnresolvedId = messageId;
        resolved[index] = ordinal;
    }

    // OpenCode can persist the live assistant suffix after this snapshot is captured.
    // Provisional suffix ordinals keep active passes flowing; when those rows appear,
    // the incremental reader verifies that their persisted ordinals agree.
    let suffixStart = annotated.length;
    while (suffixStart > 0 && resolved[suffixStart - 1] === undefined) suffixStart -= 1;
    for (let index = 0; index < suffixStart; index += 1) {
        if (resolved[index] === undefined) {
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
        const messageId = getMessageId(visibleMessages[index]) as string;
        const ordinal = resolved[index] as number;
        const prior = memo.get(messageId);
        if (prior !== undefined && prior !== ordinal) {
            return { ok: false, reason: "mismatch", messageId };
        }
        memo.set(messageId, ordinal);
        annotated[index].absolute_ordinal = ordinal;
    }

    return {
        ok: true,
        annotatedInput: annotated,
        memoGeneration: args.generation,
        memoAnchor: anchor,
        memoStoredCount: storedCount,
        memoCanonicalCount: canonicalCount,
        normalizations,
    };
}

function ordinalForMessageId(args: {
    raw: RawMessageParts | null;
    messageId: string;
    declaredOrdinal: number;
    generation: number;
    state: SessionQueueState;
}): number | null | "mismatch" {
    if (args.state.idOrdinalMemoGeneration !== args.generation) {
        args.state.idOrdinalMemo.clear();
        args.state.idOrdinalMemoGeneration = args.generation;
    }
    if (!args.raw || args.raw.id !== args.messageId || args.declaredOrdinal < 1) return null;
    const prior = args.state.idOrdinalMemo.get(args.messageId);
    if (prior !== undefined && prior !== args.declaredOrdinal) return "mismatch";
    args.state.idOrdinalMemo.set(args.messageId, args.declaredOrdinal);
    return args.declaredOrdinal;
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
    compartment: ReturnType<typeof getCompartments>[number];
    readRawById: (messageId: string) => RawMessageParts | null;
    state: SessionQueueState;
}): unknown | null | "mismatch" {
    const startRaw = args.readRawById(args.compartment.startMessageId);
    const endRaw = args.readRawById(args.compartment.endMessageId);
    const startOrdinal = ordinalForMessageId({
        raw: startRaw,
        messageId: args.compartment.startMessageId,
        declaredOrdinal: args.compartment.startMessage,
        generation: args.state.shadowGeneration,
        state: args.state,
    });
    const endOrdinal = ordinalForMessageId({
        raw: endRaw,
        messageId: args.compartment.endMessageId,
        declaredOrdinal: args.compartment.endMessage,
        generation: args.state.shadowGeneration,
        state: args.state,
    });
    if (startOrdinal === "mismatch" || endOrdinal === "mismatch") return "mismatch";
    if (startOrdinal === null || endOrdinal === null) return null;
    const startCreatedAt = startRaw?.createdAt;
    const endCreatedAt = endRaw?.createdAt;
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
            startRaw ?? undefined,
            "start",
        ),
        end_message_id: flatBlockIdForRawMessage(
            args.compartment.endMessageId,
            endRaw ?? undefined,
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

async function buildStateSyncPayload(args: {
    state: SessionQueueState;
    pass: Pick<ShadowTransformPass, "db" | "sessionId" | "projectPath" | "passInputs"> & {
        declaredTrim?: ShadowDeclaredTrim | null;
    };
    force: boolean;
    shouldAbortSeed?: () => boolean;
    beforeSerializeCompartment?: () => void;
    yieldEveryCompartments?: number;
}): Promise<
    ShadowStateSyncPayload | null | "m0_mutation" | "mismatch" | "unresolved" | "seed_budget"
> {
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
    // State sync needs only compartment boundary messages. Cache point reads within
    // this payload so adjacent compartments sharing a boundary read it once.
    const rawById = new Map<string, RawMessageParts | null>();
    const readRawById = (messageId: string): RawMessageParts | null => {
        if (!rawById.has(messageId)) {
            rawById.set(messageId, readRawSessionMessagePartsById(args.pass.sessionId, messageId));
        }
        return rawById.get(messageId) ?? null;
    };
    const compartments: unknown[] = [];
    const yieldEvery = Math.max(
        1,
        args.yieldEveryCompartments ?? SHADOW_SEED_YIELD_EVERY_COMPARTMENTS,
    );
    let serializedCount = 0;
    for (const compartment of getCompartments(args.pass.db, args.pass.sessionId)) {
        if (compartment.sequence <= acked.compartment_sequence) continue;
        args.beforeSerializeCompartment?.();
        if (args.shouldAbortSeed?.()) return "seed_budget";
        const serialized = serializeCompartment({
            compartment,
            readRawById,
            state: args.state,
        });
        if (serialized === "mismatch") return "mismatch";
        if (serialized === null) return "unresolved";
        compartments.push(serialized);
        serializedCount += 1;
        if (serializedCount % yieldEvery === 0) {
            await yieldToEventLoop();
            if (args.shouldAbortSeed?.()) return "seed_budget";
        }
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
            seed_boundary_id:
                args.state.seedPassPending && compartments.length > 0
                    ? (args.pass.declaredTrim?.flat_boundary_id ?? null)
                    : null,
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

    const disableIfSeedBudgetExceeded = (sessionId: string, state: SessionQueueState): boolean => {
        const activeElapsed =
            state.seedStartedAtMs === null ? 0 : Math.max(0, seedClock() - state.seedStartedAtMs);
        if (state.seedBudgetSpentMs + activeElapsed <= seedBudgetMs) return false;
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
        if (state.running) return;
        state.running = true;
        void runQueue(sessionId, state).finally(() => {
            state.running = false;
            if (!state.skipped && state.queue.length > 0) schedule(sessionId);
        });
    };

    const pushWork = (sessionId: string, work: ShadowWorkItem): void => {
        const state = getState(sessionId);
        if (state.skipped) return;
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
    ): Promise<unknown> => {
        const controller = new AbortController();
        let timer: ReturnType<typeof setTimeout> | undefined;
        const timeout = new Promise<never>((_, reject) => {
            timer = setTimeout(() => {
                state.counters.send_timeouts += 1;
                const error = new Error(`shadow send timeout after ${sendTimeoutMs}ms`) as Error & {
                    code?: string;
                };
                error.code = "ETIMEDOUT";
                controller.abort(error);
                reject(error);
            }, sendTimeoutMs);
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
        while (!state.skipped && state.queue.length > 0) {
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
        args.state.seedBudgetSpentMs = 0;
        args.state.seedStartedAtMs = seedClock();
        const projectRoot = args.projectRoot ?? process.cwd();
        const body = toFlatWireBody(buildShadowResetBody(args));
        const response = await callTransport(args.state, {
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
        args.state.counters.resets_sent += 1;
        sessionLog(
            args.sessionId,
            `shadow: reset acknowledged (generation=${args.state.shadowGeneration})`,
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
        if (state.skipped) return;
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
                sessionLog(pass.sessionId, "shadow: capture failed (ignored):", error);
                return;
            }
        }
        if (syncPayload === "seed_budget") {
            disableIfSeedBudgetExceeded(pass.sessionId, state);
            return;
        }
        if (state.seedPassPending) {
            if (disableIfSeedBudgetExceeded(pass.sessionId, state)) return;
            state.seedStartedAtMs = null;
            state.seedBudgetSpentMs = 0;
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
            let response: unknown;
            try {
                response = await callTransport(state, {
                    sessionId: pass.sessionId,
                    projectRoot: pass.projectRoot,
                    method: "state_sync",
                    body: toFlatWireBody(syncPayload),
                });
            } catch (error) {
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
        const response = await callTransport(state, {
            sessionId: pass.sessionId,
            projectRoot: pass.projectRoot,
            method: "shadow_transform",
            body: transformBody,
        });
        if (state.skipped) return;
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

class SubcShadowTransport implements ShadowTransport {
    private readonly connectionFile: string;
    private readonly moduleId: string;
    private readonly requestTimeoutMs: number;
    private client: SubcClient | null = null;
    private routes = new Map<string, RouteHandle>();
    private activeSession: string | null = null;
    private nextProbeMs = 0;
    private backoffMs = CONNECT_BACKOFF_INITIAL_MS;
    private connectionGeneration = 0;

    constructor(
        connectionFile?: string,
        moduleId = DEFAULT_MODULE_ID,
        requestTimeoutMs = SHADOW_SEND_TIMEOUT_MS,
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
            session: `shadow:${sessionId}`,
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
