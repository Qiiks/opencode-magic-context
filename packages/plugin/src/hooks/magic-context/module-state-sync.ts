import { createHmac, randomUUID } from "node:crypto";
import {
    getMaxMemoryIdForProjects,
    getMemoriesByProject,
    getMemoriesByProjects,
} from "../../features/magic-context/memory/storage-memory";
import type { ContextDatabase } from "../../features/magic-context/storage";
import { getCompartments, getOrCreateSessionMeta } from "../../features/magic-context/storage";
import {
    getMaxMemoryMutationIdForProjects,
    getMemoryMutationsForRenderByProjects,
} from "../../features/magic-context/storage-memory-mutation-log";
import { getActiveUserMemories } from "../../features/magic-context/user-memory/storage-user-memory";
import {
    computeWorkspaceEpochFingerprint,
    expandWorkspaceIdentitySetWithAliases,
    resolveWorkspaceIdentitySet,
    resolveWorkspaceShareCategories,
} from "../../features/magic-context/workspaces";
import { getHarness } from "../../shared/harness";
import { sessionLog } from "../../shared/logger";
import { isRecord } from "../../shared/record-type-guard";
import { MODULE_PAGE_MAX_BYTES, moduleWireBodyBytes } from "./module-wire";
import {
    readRawSessionMessageOrdinalById,
    readRawSessionMessagePartsById,
} from "./read-session-chunk";
import type { RawMessageParts } from "./read-session-raw";
import { formatDate } from "./temporal-awareness";

export interface ModuleWatermarks {
    compartment_sequence: number;
    memory_id: number;
    m0_mutation_id: number;
    memory_mutation_id: number;
    last_todo_state_hash: string;
}

export interface ModuleWorkspacePayload {
    fingerprint: string;
    members: Array<{ project_path: string; share_categories: string[] }>;
}

export interface ModuleStateSyncPayload {
    method: "state_sync";
    params: {
        session_id?: string;
        shadow_generation: number;
        expected_shadow_seq: number;
        seed_id?: string;
        seed_generation?: number;
        seed_batch_index?: number;
        seed_batch_total?: number;
        seed_complete?: boolean;
        seed_boundary_id?: string | null;
        compartments: unknown[];
        memories?: unknown[];
        memory_mutations?: unknown[];
        user_profile: string[];
        workspace?: ModuleWorkspacePayload | null;
        last_todo_state?: string;
        acked_watermarks?: ModuleWatermarks;
    };
    watermarks: ModuleWatermarks;
    wireBatches?: ModuleStateSyncPayload[];
}

/** The subset of sender state needed to serialize a state-sync payload. */
export interface ModuleStateSyncState {
    shadowGeneration: number;
    lastAckedSeq: number;
    lastAckedWatermarks: ModuleWatermarks | null;
    idOrdinalMemoGeneration: number;
    idOrdinalMemo: Map<string, number>;
    seedPassPending?: boolean;
    authorityMemorySyncSkipLogged?: boolean;
}

export interface ModuleStateSyncPass {
    db: ContextDatabase;
    sessionId: string;
    projectPath?: string;
    nowMs: number;
}

export interface ModuleStateSyncOptions {
    beforeSerializeCompartment?: () => void;
    yieldEveryCompartments?: number;
    shouldAbortSeed?: () => boolean;
    /** Cached authority state used only to avoid sending rows the module already owns. */
    authorityState?: "TS" | "PREPARING" | "MODULE" | "DRAINING";
    /** Enable the authority sender's one-time durable-sequence adoption. */
    authority?: boolean;
    /** Share adoption state across every authority sync attempt in one transform pass. */
    authoritySeqAdoption?: { used: boolean };
}

export interface ModuleCompartmentMirrorRow {
    sequence: number;
    start_message: number;
    end_message: number;
    start_message_id: string;
    end_message_id: string;
    title: string;
    content: string;
    p1?: string | null;
    p2?: string | null;
    p3?: string | null;
    p4?: string | null;
    importance?: number | null;
    episode_type?: string | null;
    legacy?: number | null;
    created_at?: number;
}

export interface ModuleCompartmentMirrorResponse {
    max_sequence: number;
    compartments: ModuleCompartmentMirrorRow[];
}

/**
 * The module owns its SQLite file, so TS cannot read rows directly. This narrow
 * reader is the seam for the module's future `session.status` compartment page.
 * It deliberately returns typed rows instead of pretending the TS database is
 * authoritative for module-published content.
 */
export interface ModuleCompartmentReader {
    getCompartmentsAfter(
        sessionId: string,
        afterSequence: number,
    ): Promise<ModuleCompartmentMirrorResponse>;
}

export async function mirrorModuleCompartments(args: {
    db: ContextDatabase;
    sessionId: string;
    reader: ModuleCompartmentReader;
}): Promise<number> {
    const row = args.db
        .prepare(
            "SELECT COALESCE(MAX(sequence), -1) AS max_sequence FROM compartments WHERE session_id = ?",
        )
        .get(args.sessionId) as { max_sequence?: number } | undefined;
    const afterSequence = row?.max_sequence ?? -1;
    const published = await args.reader.getCompartmentsAfter(args.sessionId, afterSequence);
    if (!Number.isFinite(published.max_sequence) || published.max_sequence <= afterSequence) {
        return afterSequence;
    }
    const insert = args.db.prepare(
        "INSERT INTO compartments (session_id, sequence, start_message, end_message, start_message_id, end_message_id, title, content, p1, p2, p3, p4, importance, episode_type, legacy, created_at, harness) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT(session_id, sequence) DO NOTHING",
    );
    const now = Date.now();
    args.db.transaction(() => {
        for (const compartment of published.compartments) {
            if (
                !Number.isFinite(compartment.sequence) ||
                compartment.sequence <= afterSequence ||
                typeof compartment.start_message_id !== "string" ||
                typeof compartment.end_message_id !== "string" ||
                typeof compartment.title !== "string" ||
                typeof compartment.content !== "string"
            ) {
                continue;
            }
            insert.run(
                args.sessionId,
                compartment.sequence,
                compartment.start_message,
                compartment.end_message,
                compartment.start_message_id,
                compartment.end_message_id,
                compartment.title,
                compartment.content,
                compartment.p1 ?? null,
                compartment.p2 ?? null,
                compartment.p3 ?? null,
                compartment.p4 ?? null,
                compartment.importance ?? 50,
                compartment.episode_type ?? null,
                compartment.legacy ?? (compartment.p1 ? 0 : 1),
                compartment.created_at ?? now,
                getHarness(),
            );
        }
    })();
    return published.max_sequence;
}

interface ModuleWorkspaceContext {
    workspace: ModuleWorkspacePayload | null;
    expandedIdentities: string[];
    ownIdentities: string[];
    shareCategories: string[] | null;
}

function stableHash(value: string): string {
    return createHmac("sha256", "magic-context-shadow-watermark").update(value).digest("hex");
}

function yieldToEventLoop(): Promise<void> {
    return new Promise((resolve) => setTimeout(resolve, 0));
}

function resolveModuleWorkspaceContext(
    db: ContextDatabase,
    projectPath?: string,
): ModuleWorkspaceContext {
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

export function loadModuleWatermarks(args: {
    db: ContextDatabase;
    sessionId: string;
    projectPath?: string;
}): ModuleWatermarks {
    const workspace = resolveModuleWorkspaceContext(args.db, args.projectPath);
    const sessionMeta = getOrCreateSessionMeta(args.db, args.sessionId);
    const compartmentRow = args.db
        .prepare(
            "SELECT COALESCE(MAX(sequence), -1) AS max_sequence FROM compartments WHERE session_id = ?",
        )
        .get(args.sessionId) as { max_sequence?: number } | undefined;
    const memoryId = args.projectPath
        ? getMaxMemoryIdForProjects(
              args.db,
              workspace.expandedIdentities,
              workspace.ownIdentities,
              workspace.shareCategories,
          )
        : 0;
    const m0Row = args.db
        .prepare("SELECT COALESCE(MAX(id), 0) AS max_id FROM m0_mutation_log WHERE session_id = ?")
        .get(args.sessionId) as { max_id?: number } | undefined;
    const memoryMutationId = args.projectPath
        ? (getMaxMemoryMutationIdForProjects(args.db, workspace.expandedIdentities) ?? 0)
        : 0;
    return {
        compartment_sequence: compartmentRow?.max_sequence ?? -1,
        memory_id: memoryId,
        m0_mutation_id: m0Row?.max_id ?? 0,
        memory_mutation_id: memoryMutationId,
        last_todo_state_hash: stableHash(sessionMeta.lastTodoState ?? ""),
    };
}

export function moduleWatermarksEqual(
    left: ModuleWatermarks | null,
    right: ModuleWatermarks,
): boolean {
    return (
        left !== null &&
        left.compartment_sequence === right.compartment_sequence &&
        left.memory_id === right.memory_id &&
        left.m0_mutation_id === right.m0_mutation_id &&
        left.memory_mutation_id === right.memory_mutation_id &&
        left.last_todo_state_hash === right.last_todo_state_hash
    );
}

function flatBlockCountForRawMessage(message: RawMessageParts | null): number {
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
                const state = isRecord(part.state) ? part.state : undefined;
                const status = state?.status;
                const hasCompletedStatus = status === "completed" || status === "error";
                const hasOutput = state
                    ? typeof state.output === "string" || typeof state.error === "string"
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

function flatBlockIdForRawMessage(
    messageId: string,
    raw: RawMessageParts | null,
    edge: "start" | "end",
): string {
    const blockIndex = edge === "start" ? 0 : flatBlockCountForRawMessage(raw) - 1;
    return `${messageId}#${blockIndex}`;
}

/**
 * Compartment rows retain ordinals from the TS storage basis, which can include
 * synthetic summary rows. Resolve module boundaries from the summary-excluding
 * basis so the shared memo compares one canonical value everywhere.
 */
export function canonicalOrdinalForMessageId(args: {
    sessionId: string;
    raw: RawMessageParts | null;
    messageId: string;
    generation: number;
    state: ModuleStateSyncState;
}): number | null | "mismatch" {
    if (args.state.idOrdinalMemoGeneration !== args.generation) {
        args.state.idOrdinalMemo.clear();
        args.state.idOrdinalMemoGeneration = args.generation;
    }
    if (!args.raw || args.raw.id !== args.messageId) return null;
    const prior = args.state.idOrdinalMemo.get(args.messageId);
    if (prior !== undefined) return prior;
    const canonical = readRawSessionMessageOrdinalById(args.sessionId, args.messageId);
    if (canonical === null || canonical < 1) return null;
    args.state.idOrdinalMemo.set(args.messageId, canonical);
    return canonical;
}

function serializeCompartment(args: {
    compartment: ReturnType<typeof getCompartments>[number];
    sessionId: string;
    readRawById: (messageId: string) => RawMessageParts | null;
    state: ModuleStateSyncState;
}): unknown | null | "mismatch" {
    const startRaw = args.readRawById(args.compartment.startMessageId);
    const endRaw = args.readRawById(args.compartment.endMessageId);
    const startOrdinal = canonicalOrdinalForMessageId({
        sessionId: args.sessionId,
        raw: startRaw,
        messageId: args.compartment.startMessageId,
        generation: args.state.shadowGeneration,
        state: args.state,
    });
    const endOrdinal = canonicalOrdinalForMessageId({
        sessionId: args.sessionId,
        raw: endRaw,
        messageId: args.compartment.endMessageId,
        generation: args.state.shadowGeneration,
        state: args.state,
    });
    if (startOrdinal === "mismatch" || endOrdinal === "mismatch") return "mismatch";
    if (startOrdinal === null || endOrdinal === null) return null;
    const startCreatedAt = startRaw?.createdAt;
    const endCreatedAt = endRaw?.createdAt;
    const dateRange =
        typeof startCreatedAt === "number" && typeof endCreatedAt === "number"
            ? { start_date: formatDate(startCreatedAt), end_date: formatDate(endCreatedAt) }
            : {};
    return {
        sequence: args.compartment.sequence,
        start_message: startOrdinal,
        end_message: endOrdinal,
        start_message_id: flatBlockIdForRawMessage(
            args.compartment.startMessageId,
            startRaw,
            "start",
        ),
        end_message_id: flatBlockIdForRawMessage(args.compartment.endMessageId, endRaw, "end"),
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

function seedBoundaryFromSerializedCompartments(compartments: unknown[]): string | null {
    const serialized = compartments
        .filter(isRecord)
        .filter(
            (compartment) =>
                typeof compartment.sequence === "number" &&
                typeof compartment.end_message_id === "string",
        );
    serialized.sort((left, right) => (left.sequence as number) - (right.sequence as number));
    const tail = serialized.at(-1);
    return typeof tail?.end_message_id === "string" ? tail.end_message_id : null;
}

type SeedItem =
    | { kind: "compartment"; value: unknown }
    | { kind: "memory"; value: unknown }
    | { kind: "memory_mutation"; value: unknown }
    | { kind: "user_profile"; value: string };

export function buildPagedModuleStateSyncPayloads(args: {
    shadowGeneration: number;
    expectedShadowSeq: number;
    seedId: string;
    seedBoundaryId: string | null;
    compartments: unknown[];
    memories: unknown[];
    memoryMutations: unknown[];
    userProfile: string[];
    workspace: ModuleWorkspacePayload | null;
    lastTodoState: string;
    watermarks: ModuleWatermarks;
    omitAuthorityMemorySections?: boolean;
}): ModuleStateSyncPayload[] {
    const items: SeedItem[] = [
        ...args.compartments.map((value) => ({ kind: "compartment", value }) as const),
        ...(args.omitAuthorityMemorySections
            ? []
            : args.memories.map((value) => ({ kind: "memory", value }) as const)),
        ...(args.omitAuthorityMemorySections
            ? []
            : args.memoryMutations.map((value) => ({ kind: "memory_mutation", value }) as const)),
        ...args.userProfile.map((value) => ({ kind: "user_profile", value }) as const),
    ];
    const makePayload = (input: {
        index: number;
        total: number;
        complete: boolean;
        compartments: unknown[];
        memories: unknown[];
        memoryMutations: unknown[];
        userProfile: string[];
    }): ModuleStateSyncPayload => ({
        method: "state_sync",
        params: {
            shadow_generation: args.shadowGeneration,
            expected_shadow_seq: args.expectedShadowSeq,
            seed_id: args.seedId,
            seed_generation: args.shadowGeneration,
            seed_batch_index: input.index,
            seed_batch_total: input.total,
            seed_complete: input.complete,
            compartments: input.compartments,
            ...(args.omitAuthorityMemorySections
                ? {}
                : {
                      memories: input.memories,
                      memory_mutations: input.memoryMutations,
                  }),
            user_profile: input.userProfile,
            ...(input.complete
                ? {
                      seed_boundary_id: args.seedBoundaryId,
                      workspace: args.workspace,
                      last_todo_state: args.lastTodoState,
                      acked_watermarks: args.watermarks,
                  }
                : {}),
        },
        watermarks: args.watermarks,
    });
    const appendItem = (
        batch: {
            compartments: unknown[];
            memories: unknown[];
            memoryMutations: unknown[];
            userProfile: string[];
        },
        item: SeedItem,
    ): void => {
        if (item.kind === "compartment") batch.compartments.push(item.value);
        else if (item.kind === "memory") batch.memories.push(item.value);
        else if (item.kind === "memory_mutation") batch.memoryMutations.push(item.value);
        else batch.userProfile.push(item.value);
    };

    let assumedTotal = 1;
    for (let attempt = 0; attempt < 10; attempt += 1) {
        const batches: ModuleStateSyncPayload[] = [];
        let current = {
            compartments: [],
            memories: [],
            memoryMutations: [],
            userProfile: [],
        } as {
            compartments: unknown[];
            memories: unknown[];
            memoryMutations: unknown[];
            userProfile: string[];
        };
        for (let itemIndex = 0; itemIndex < items.length; itemIndex += 1) {
            const candidate = {
                compartments: [...current.compartments],
                memories: [...current.memories],
                memoryMutations: [...current.memoryMutations],
                userProfile: [...current.userProfile],
            };
            appendItem(candidate, items[itemIndex]);
            const complete = itemIndex + 1 === items.length;
            const candidatePayload = makePayload({
                index: batches.length,
                total: assumedTotal,
                complete,
                ...candidate,
            });
            if (
                moduleWireBodyBytes({ method: "state_sync", params: candidatePayload.params }) <=
                MODULE_PAGE_MAX_BYTES
            ) {
                current = candidate;
                continue;
            }
            const currentHasItems = Object.values(current).some((values) => values.length > 0);
            if (currentHasItems) {
                batches.push(
                    makePayload({
                        index: batches.length,
                        total: assumedTotal,
                        complete: false,
                        ...current,
                    }),
                );
            }
            current = { compartments: [], memories: [], memoryMutations: [], userProfile: [] };
            appendItem(current, items[itemIndex]);
            const itemOnlyPayload = makePayload({
                index: batches.length,
                total: assumedTotal,
                complete: false,
                ...current,
            });
            if (
                moduleWireBodyBytes({ method: "state_sync", params: itemOnlyPayload.params }) >
                MODULE_PAGE_MAX_BYTES
            ) {
                throw new Error("module seed item exceeds the 512 KiB batch limit");
            }
            if (complete) {
                const itemWithTailPayload = makePayload({
                    index: batches.length,
                    total: assumedTotal,
                    complete: true,
                    ...current,
                });
                if (
                    moduleWireBodyBytes({
                        method: "state_sync",
                        params: itemWithTailPayload.params,
                    }) > MODULE_PAGE_MAX_BYTES
                ) {
                    batches.push(itemOnlyPayload);
                    current = {
                        compartments: [],
                        memories: [],
                        memoryMutations: [],
                        userProfile: [],
                    };
                }
            }
        }
        const finalPayload = makePayload({
            index: batches.length,
            total: assumedTotal,
            complete: true,
            ...current,
        });
        if (
            moduleWireBodyBytes({ method: "state_sync", params: finalPayload.params }) >
            MODULE_PAGE_MAX_BYTES
        ) {
            throw new Error("module seed scalar tail exceeds the 512 KiB batch limit");
        }
        batches.push(finalPayload);
        if (batches.length === assumedTotal) return batches;
        assumedTotal = batches.length;
    }
    throw new Error("module seed batch count did not stabilize");
}

export async function buildModuleStateSyncPayload(args: {
    state: ModuleStateSyncState;
    pass: ModuleStateSyncPass;
    force: boolean;
    options?: ModuleStateSyncOptions;
    seedId?: string;
}): Promise<
    ModuleStateSyncPayload | null | "m0_mutation" | "mismatch" | "unresolved" | "seed_budget"
> {
    const workspace = resolveModuleWorkspaceContext(args.pass.db, args.pass.projectPath);
    // One authority pool has one writer. While MODULE owns memories, this sender only mirrors
    // module changes back to TypeScript and must not send the TypeScript view in the other direction.
    const omitAuthorityMemorySections = args.options?.authorityState === "MODULE";
    const currentWatermarks = loadModuleWatermarks({
        db: args.pass.db,
        sessionId: args.pass.sessionId,
        projectPath: args.pass.projectPath,
    });
    if (
        !args.force &&
        args.state.lastAckedWatermarks &&
        currentWatermarks.m0_mutation_id > args.state.lastAckedWatermarks.m0_mutation_id
    ) {
        return "m0_mutation";
    }
    if (!args.force && moduleWatermarksEqual(args.state.lastAckedWatermarks, currentWatermarks)) {
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
    const rawById = new Map<string, RawMessageParts | null>();
    const readRawById = (messageId: string): RawMessageParts | null => {
        if (!rawById.has(messageId)) {
            rawById.set(messageId, readRawSessionMessagePartsById(args.pass.sessionId, messageId));
        }
        return rawById.get(messageId) ?? null;
    };
    const compartments: unknown[] = [];
    let serializedCount = 0;
    for (const compartment of getCompartments(args.pass.db, args.pass.sessionId)) {
        if (compartment.sequence <= acked.compartment_sequence) continue;
        args.options?.beforeSerializeCompartment?.();
        if (args.options?.shouldAbortSeed?.()) return "seed_budget";
        const serialized = serializeCompartment({
            compartment,
            sessionId: args.pass.sessionId,
            readRawById,
            state: args.state,
        });
        if (serialized === "mismatch") return "mismatch";
        if (serialized === null) return "unresolved";
        compartments.push(serialized);
        serializedCount += 1;
        const yieldEvery = Math.max(1, args.options?.yieldEveryCompartments ?? 10);
        if (serializedCount % yieldEvery === 0) {
            await yieldToEventLoop();
            if (args.options?.shouldAbortSeed?.()) return "seed_budget";
        }
    }

    const allMemories =
        !omitAuthorityMemorySections && args.pass.projectPath
            ? workspace.workspace
                ? getMemoriesByProjects(
                      args.pass.db,
                      workspace.expandedIdentities,
                      ["active", "permanent"],
                      args.pass.nowMs,
                      workspace.ownIdentities,
                      workspace.shareCategories,
                  )
                : getMemoriesByProject(
                      args.pass.db,
                      args.pass.projectPath,
                      ["active", "permanent"],
                      args.pass.nowMs,
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
    const userProfile = getActiveUserMemories(args.pass.db).map((memory) => memory.content);
    const memoryMutations =
        !omitAuthorityMemorySections && args.pass.projectPath
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
    const payloadArgs = {
        shadowGeneration: args.state.shadowGeneration,
        expectedShadowSeq: args.state.lastAckedSeq,
        seedId: args.seedId ?? randomUUID(),
        seedBoundaryId:
            args.state.seedPassPending === true
                ? seedBoundaryFromSerializedCompartments(compartments)
                : null,
        compartments,
        memories,
        memoryMutations,
        userProfile,
        workspace: workspace.workspace,
        lastTodoState: sessionMeta.lastTodoState ?? "",
        watermarks: currentWatermarks,
        omitAuthorityMemorySections,
    };
    if (args.force) {
        const wireBatches = buildPagedModuleStateSyncPayloads(payloadArgs);
        return { ...wireBatches[0], wireBatches };
    }
    return {
        method: "state_sync",
        params: {
            shadow_generation: args.state.shadowGeneration,
            expected_shadow_seq: args.state.lastAckedSeq,
            compartments,
            ...(omitAuthorityMemorySections ? {} : { memories, memory_mutations: memoryMutations }),
            user_profile: userProfile,
            workspace: workspace.workspace,
            last_todo_state: sessionMeta.lastTodoState ?? "",
            acked_watermarks: currentWatermarks,
        },
        watermarks: currentWatermarks,
    };
}

export interface ModuleStateSyncClient {
    call(args: {
        sessionId: string;
        projectRoot: string;
        method:
            | "state_sync"
            | "transform"
            | "session.status"
            | "session.flush"
            | "session.recomp"
            | "session.wrapup"
            | "todo_state.set"
            | "agent_drops.append"
            | "ctx_note"
            | "ctx_memory"
            | "note.evaluate"
            | "transform.ack"
            | "transform.nack";
        body: unknown;
        signal?: AbortSignal;
    }): Promise<unknown>;
}

function responseMemoriesSkipped(response: unknown): boolean {
    const value = isRecord(response) && isRecord(response.result) ? response.result : response;
    return isRecord(value) && value.memories_skipped === true;
}

function readAuthoritySeqMismatch(error: unknown): number | null {
    let current = error;
    const seen = new Set<unknown>();
    while (isRecord(current) && !seen.has(current)) {
        seen.add(current);
        if (current.code === "authority_seq_mismatch") {
            const direct = current.durable_authority_seq;
            if (typeof direct === "number" && Number.isSafeInteger(direct) && direct >= 0) {
                return direct;
            }
            if (typeof current.message === "string") {
                try {
                    const details: unknown = JSON.parse(current.message);
                    if (isRecord(details) && details.code === "authority_seq_mismatch") {
                        const durable = details.durable_authority_seq;
                        if (
                            typeof durable === "number" &&
                            Number.isSafeInteger(durable) &&
                            durable >= 0
                        ) {
                            return durable;
                        }
                    }
                } catch {
                    // Older transports only expose the typed code and human message.
                }
            }
        }
        current = current.cause;
    }
    return null;
}

/**
 * Mode-neutral state synchronization: the same watermark-triggered assembly is
 * used by the mirror sender and the Rust authority path. Callers own retries and
 * lineage handling because shadow and authority have different failure policy.
 */
export async function syncModuleState(args: {
    client: ModuleStateSyncClient;
    state: ModuleStateSyncState;
    pass: ModuleStateSyncPass;
    projectRoot: string;
    force: boolean;
    options?: ModuleStateSyncOptions;
}): Promise<ModuleWatermarks | null> {
    let force = args.force;
    const adoption = args.options?.authoritySeqAdoption ?? { used: false };
    for (;;) {
        const payload = await buildModuleStateSyncPayload({
            state: args.state,
            pass: args.pass,
            force,
            options: args.options,
        });
        if (payload === null) return null;
        if (
            payload === "m0_mutation" ||
            payload === "mismatch" ||
            payload === "unresolved" ||
            payload === "seed_budget"
        ) {
            throw new Error(`module state sync ${payload}`);
        }
        try {
            const batches = payload.wireBatches ?? [payload];
            for (const batch of batches) {
                const response = await args.client.call({
                    sessionId: args.pass.sessionId,
                    projectRoot: args.projectRoot,
                    method: "state_sync",
                    body: {
                        method: batch.method,
                        ...batch.params,
                    },
                });
                if (
                    args.options?.authority === true &&
                    responseMemoriesSkipped(response) &&
                    !args.state.authorityMemorySyncSkipLogged
                ) {
                    args.state.authorityMemorySyncSkipLogged = true;
                    sessionLog(
                        args.pass.sessionId,
                        "authority state sync skipped module-owned memory sections",
                    );
                }
            }
        } catch (error) {
            const durableSeq = args.options?.authority ? readAuthoritySeqMismatch(error) : null;
            if (durableSeq === null || adoption.used) throw error;
            adoption.used = true;
            args.state.lastAckedSeq = durableSeq;
            // A fresh authority process only knows its in-memory sequence. After adopting the
            // durable sequence, discard sender watermarks and force a full rebuild because it
            // cannot know which durable rows the sequence covers.
            args.state.lastAckedWatermarks = null;
            force = true;
            continue;
        }
        args.state.lastAckedWatermarks = payload.watermarks;
        args.state.lastAckedSeq += 1;
        return payload.watermarks;
    }
}

export const __moduleStateSyncTest = {
    buildModuleStateSyncPayload,
    buildPagedModuleStateSyncPayloads,
    canonicalOrdinalForMessageId,
    loadModuleWatermarks,
    moduleWatermarksEqual,
    syncModuleState,
};
