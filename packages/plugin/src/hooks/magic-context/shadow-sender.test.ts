/// <reference types="bun-types" />

import { afterEach, describe, expect, it } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { SocketTimeoutError, SubcError } from "@cortexkit/subc-client";
import {
    appendCompartments,
    replaceAllCompartmentState,
} from "../../features/magic-context/compartment-storage";
import { insertMemory } from "../../features/magic-context/memory/storage-memory";
import {
    type ContextDatabase,
    closeDatabase,
    openDatabase,
} from "../../features/magic-context/storage";
import { queueM0Mutation } from "../../features/magic-context/storage-m0-mutation-log";
import {
    appendAutoSearchHintDecision,
    setPersistedCompactionMarkerState,
} from "../../features/magic-context/storage-meta-persisted";
import { insertUserMemory } from "../../features/magic-context/user-memory/storage-user-memory";
import { Database } from "../../shared/sqlite";
import { closeQuietly } from "../../shared/sqlite-helpers";
import { setRawMessageProvider } from "./read-session-chunk";
import {
    __shadowSenderTest,
    createShadowSender,
    type ShadowTransformPass,
    type ShadowTransport,
} from "./shadow-sender";
import type { MessageLike, TagNormalizationTarget } from "./tag-messages";

const tempDirs: string[] = [];
const originalXdgDataHome = process.env.XDG_DATA_HOME;
const originalXdgCacheHome = process.env.XDG_CACHE_HOME;

afterEach(() => {
    closeDatabase();
    if (originalXdgDataHome === undefined) delete process.env.XDG_DATA_HOME;
    else process.env.XDG_DATA_HOME = originalXdgDataHome;
    if (originalXdgCacheHome === undefined) delete process.env.XDG_CACHE_HOME;
    else process.env.XDG_CACHE_HOME = originalXdgCacheHome;
    for (const dir of tempDirs) {
        rmSync(dir, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
    }
    tempDirs.length = 0;
});

function useTempDataHome(prefix: string): void {
    const dir = mkdtempSync(join(tmpdir(), prefix));
    tempDirs.push(dir);
    process.env.XDG_DATA_HOME = dir;
    process.env.XDG_CACHE_HOME = dir;
}

function createOpenCodeDb(
    sessionId: string,
    messages: Array<{ id: string; role: string; text: string; summary?: boolean }>,
): void {
    const dbPath = join(process.env.XDG_DATA_HOME ?? "", "opencode", "opencode.db");
    mkdirSync(dirname(dbPath), { recursive: true });
    const db = new Database(dbPath);
    try {
        db.exec(`
            CREATE TABLE IF NOT EXISTS message (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL,
                data TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS part (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                message_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL,
                data TEXT NOT NULL
            );
        `);
        db.prepare("DELETE FROM part WHERE session_id = ?").run(sessionId);
        db.prepare("DELETE FROM message WHERE session_id = ?").run(sessionId);
        const insertMessage = db.prepare(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES (?, ?, ?, ?, ?)",
        );
        const insertPart = db.prepare(
            "INSERT INTO part (message_id, session_id, time_created, time_updated, data) VALUES (?, ?, ?, ?, ?)",
        );
        messages.forEach((message, index) => {
            const timestamp = new Date(2025, 0, index + 2, 12).getTime();
            insertMessage.run(
                message.id,
                sessionId,
                timestamp,
                timestamp,
                JSON.stringify({
                    id: message.id,
                    role: message.role,
                    sessionID: sessionId,
                    summary: message.summary === true ? true : undefined,
                    finish: message.summary === true ? "stop" : undefined,
                }),
            );
            insertPart.run(
                message.id,
                sessionId,
                timestamp,
                timestamp,
                JSON.stringify({ type: "text", text: message.text }),
            );
        });
    } finally {
        closeQuietly(db);
    }
}

function basePass(args: {
    db: ContextDatabase;
    sessionId: string;
    inputMessages?: MessageLike[];
    outputMessages?: MessageLike[];
    normalizationTargets?: TagNormalizationTarget[];
    projectPath?: string;
    nowMs?: number;
    isSubagent?: boolean;
    midTurn?: boolean;
}): ShadowTransformPass {
    const inputMessages = args.inputMessages ?? [message(args.sessionId, "m1", "hello")];
    return {
        sessionId: args.sessionId,
        isSubagent: args.isSubagent ?? false,
        db: args.db,
        inputMessages,
        outputMessages: args.outputMessages ?? structuredClone(inputMessages),
        normalizationTargets: args.normalizationTargets ?? [],
        projectRoot: process.cwd(),
        projectPath: args.projectPath ?? "/tmp/project",
        passInputs: {
            now_ms: args.nowMs ?? 1,
            model_key: "test/provider",
            usage: { input_tokens: 10, limit: 100 },
            effective_execute_threshold: 65,
            history_budget_tokens: 19_500,
            cache_ttl: "ephemeral",
            mid_turn: args.midTurn ?? false,
        },
        tsDecision: { class: "defer", marker_state: { advanced_this_pass: false } },
        declaredTrimBefore: null,
    };
}

function message(sessionId: string, id: string, text: string): MessageLike {
    return {
        info: { id, role: "user", sessionID: sessionId },
        parts: [{ type: "text", text }],
    };
}

function summaryMessage(sessionId: string, id: string): MessageLike {
    return {
        info: { id, role: "assistant", sessionID: sessionId, summary: true, finish: "stop" },
        parts: [{ type: "text", text: "internal summary" }],
    };
}

class FakeTransport implements ShadowTransport {
    calls: Array<{ method: string; body: unknown }> = [];
    syncFailuresRemaining = 0;
    failSyncAtCall = 0;
    syncCalls = 0;
    closedSessions: string[] = [];
    seedBoundaryFailuresRemaining = 0;
    rejectNextTransform = false;
    transformFailuresRemaining = 0;
    transformFailureCode = "shadow_identity_drift";
    quarantinedResponsesRemaining = 0;
    resetFailuresRemaining = 0;
    resetTimeoutAtCall = 0;
    resetTimeoutError: Error | null = null;
    resetCalls = 0;
    seq = 0;
    releaseReset: (() => void) | null = null;
    releaseTransform: (() => void) | null = null;
    private resetGate: Promise<void> | null = null;
    private transformGate: Promise<void> | null = null;

    blockFirstReset(): void {
        this.resetGate = new Promise((resolve) => {
            this.releaseReset = resolve;
        });
    }

    blockFirstTransformPage(): void {
        this.transformGate = new Promise((resolve) => {
            this.releaseTransform = resolve;
        });
    }

    async call(req: { method: string; body: unknown }): Promise<unknown> {
        this.calls.push({ method: req.method, body: req.body });
        if (req.method === "shadow_reset") {
            this.resetCalls += 1;
            if (this.resetCalls === this.resetTimeoutAtCall) {
                throw this.resetTimeoutError ?? new Error("read timeout");
            }
        }
        if (req.method === "shadow_reset" && this.resetGate) {
            await this.resetGate;
            this.resetGate = null;
        }
        if (req.method === "shadow_transform" && this.transformGate) {
            const body = req.body as { transform_page_index?: number };
            if (body.transform_page_index === 0) {
                const gate = this.transformGate;
                this.transformGate = null;
                await gate;
            }
        }
        if (req.method === "shadow_reset" && this.resetFailuresRemaining > 0) {
            this.resetFailuresRemaining -= 1;
            throw new Error("reset interrupted");
        }
        if (req.method === "state_sync") {
            this.syncCalls += 1;
            if (this.syncCalls === this.failSyncAtCall) {
                const error = new Error("transport busy") as Error & { code?: string };
                error.code = "EBUSY";
                throw error;
            }
            if (this.syncFailuresRemaining > 0) {
                this.syncFailuresRemaining -= 1;
                throw new Error("dropped sync");
            }
        }
        if (req.method === "state_sync" && this.seedBoundaryFailuresRemaining > 0) {
            this.seedBoundaryFailuresRemaining -= 1;
            throw new SubcError("stale seed boundary", "shadow_seed_boundary_mismatch");
        }
        if (req.method === "shadow_transform" && this.rejectNextTransform) {
            this.rejectNextTransform = false;
            throw new SubcError("generation mismatch", "stale_generation");
        }
        if (req.method === "shadow_transform" && this.transformFailuresRemaining > 0) {
            this.transformFailuresRemaining -= 1;
            throw new SubcError("CK message block identity drift", this.transformFailureCode);
        }
        if (req.method === "shadow_reset") {
            this.seq = 0;
            return { result: { shadow_generation: 1, shadow_seq: 0 } };
        }
        if (req.method === "state_sync") {
            const body = req.body as { seed_complete?: boolean; seed_id?: string };
            if (body.seed_id && body.seed_complete !== true) {
                return { result: { ok: true, staged: true } };
            }
            this.seq += 1;
            return { result: { shadow_seq: this.seq } };
        }
        if (req.method === "shadow_transform") {
            this.seq += 1;
            if (this.quarantinedResponsesRemaining > 0) {
                this.quarantinedResponsesRemaining -= 1;
                return { result: { shadow_seq: this.seq, quarantined: true } };
            }
            return { result: { shadow_seq: this.seq, quarantined: false } };
        }
        return { result: {} };
    }

    closeSession(sessionId: string): void {
        this.closedSessions.push(sessionId);
    }
}

class StallingSyncTransport implements ShadowTransport {
    methods: string[] = [];
    transformBodies: unknown[] = [];
    retainedBody: unknown | null = null;
    private stalled = false;
    private seq = 0;

    async call(req: {
        method: "shadow_reset" | "state_sync" | "shadow_transform";
        body: unknown;
        signal?: AbortSignal;
    }): Promise<unknown> {
        this.methods.push(req.method);
        if (req.method === "shadow_reset") {
            this.seq = 0;
            return { result: { shadow_generation: 1, shadow_seq: 0 } };
        }
        if (req.method === "state_sync" && !this.stalled) {
            this.stalled = true;
            this.retainedBody = req.body;
            return await new Promise((_, reject) => {
                req.signal?.addEventListener(
                    "abort",
                    () => {
                        this.retainedBody = null;
                        reject(req.signal?.reason ?? new Error("aborted"));
                    },
                    { once: true },
                );
            });
        }
        this.seq += 1;
        if (req.method === "shadow_transform") this.transformBodies.push(req.body);
        return { result: { shadow_seq: this.seq, quarantined: false } };
    }
}

async function waitFor(predicate: () => boolean): Promise<void> {
    const deadline = Date.now() + 5_000;
    while (Date.now() < deadline) {
        if (predicate()) return;
        await new Promise((resolve) => setTimeout(resolve, 5));
    }
    expect(predicate()).toBe(true);
}

function appendLinearCompartments(db: ContextDatabase, sessionId: string, count: number): void {
    appendCompartments(
        db,
        sessionId,
        Array.from({ length: count }, (_, index) => ({
            sequence: index,
            startMessage: index + 1,
            endMessage: index + 1,
            startMessageId: `m${index + 1}`,
            endMessageId: `m${index + 1}`,
            title: `Compartment ${index + 1}`,
            content: `content ${index + 1}`,
        })),
    );
}

function installLinearRawProvider(
    sessionId: string,
    count: number,
): {
    counters: {
        fullReads: number;
        ordinalPageReads: number;
        storedCounts: number;
        partReads: number;
    };
    unregister: () => void;
} {
    const counters = { fullReads: 0, ordinalPageReads: 0, storedCounts: 0, partReads: 0 };
    const rows = Array.from({ length: count }, (_, index) => ({
        id: `m${index + 1}`,
        timeCreated: index + 1,
        contributesOrdinal: true,
        hasValidInfo: true,
    }));
    const unregister = setRawMessageProvider(sessionId, {
        readMessages() {
            counters.fullReads += 1;
            throw new Error("shadow seed must not hydrate the full raw session");
        },
        readMessageOrdinalPage(after, limit) {
            counters.ordinalPageReads += 1;
            return rows
                .filter(
                    (row) =>
                        !after ||
                        row.timeCreated > after.timeCreated ||
                        (row.timeCreated === after.timeCreated && row.id > after.id),
                )
                .slice(0, limit);
        },
        getStoredMessageCount() {
            counters.storedCounts += 1;
            return rows.length;
        },
        readMessagePartsById(messageId) {
            counters.partReads += 1;
            const row = rows.find((candidate) => candidate.id === messageId);
            return row
                ? {
                      id: row.id,
                      role: "user",
                      parts: [{ type: "text", text: row.id }],
                      createdAt: row.timeCreated,
                  }
                : null;
        },
    });
    return { counters, unregister };
}

describe("shadow sender", () => {
    it("never sends traffic for subagent sessions", async () => {
        useTempDataHome("shadow-subagent-");
        const sessionId = "s-subagent";
        createOpenCodeDb(sessionId, [{ id: "m1", role: "user", text: "child prompt" }]);
        const db = openDatabase();
        const transport = new FakeTransport();
        const sender = createShadowSender({ transport });

        sender.enqueue(basePass({ db, sessionId, isSubagent: true }));
        sender.enqueue(basePass({ db, sessionId, isSubagent: true, nowMs: 2 }));
        sender.resetSession(sessionId, "must_stay_disarmed");
        await new Promise((resolve) => setTimeout(resolve, 10));

        expect(transport.calls).toEqual([]);
    });

    it("parks after two consecutive deterministic reset reasons and sends nothing later", async () => {
        const transport = new FakeTransport();
        const sender = createShadowSender({ transport });
        const sessionId = "s-park-repeat";

        sender.resetSession(sessionId, "ordinal_mismatch");
        await waitFor(() => transport.resetCalls === 1);
        sender.resetSession(sessionId, "ordinal_mismatch");
        await waitFor(() => sender.getStats(sessionId).parked === 1);

        sender.resetSession(sessionId, "ordinal_mismatch");
        expect(transport.resetCalls).toBe(1);
        expect(sender.getStats(sessionId).parked).toBe(1);
    });

    it("parks repeated deterministic send failures and recovers after an explicit reset", async () => {
        useTempDataHome("shadow-send-failure-park-");
        const sessionId = "s-send-failure-park";
        createOpenCodeDb(sessionId, [{ id: "m1", role: "user", text: "hello" }]);
        const db = openDatabase();
        if (!db) throw new Error("test database failed to open");
        const transport = new FakeTransport();
        transport.transformFailuresRemaining = 3;
        const sender = createShadowSender({ transport });

        for (let attempt = 1; attempt <= 3; attempt += 1) {
            sender.enqueue(basePass({ db, sessionId, nowMs: attempt }));
            await waitFor(
                () =>
                    transport.calls.filter((call) => call.method === "shadow_transform").length ===
                    attempt,
            );
        }
        expect(sender.getStats(sessionId).parked).toBe(1);
        expect(sender.getStats(sessionId).send_failures).toBe(3);

        sender.enqueue(basePass({ db, sessionId, nowMs: 4 }));
        await new Promise((resolve) => setTimeout(resolve, 20));
        expect(transport.calls.filter((call) => call.method === "shadow_transform")).toHaveLength(
            3,
        );

        sender.resetSession(sessionId, "operator_reset");
        await waitFor(() => transport.resetCalls === 2);
        sender.enqueue(basePass({ db, sessionId, nowMs: 5 }));
        await waitFor(
            () => transport.calls.filter((call) => call.method === "shadow_transform").length === 4,
        );
        expect(sender.getStats(sessionId).parked).toBe(1);
        expect(sender.getStats(sessionId).transforms_sent).toBe(1);
    });

    it("does not park repeated transient route reopen resets", async () => {
        const transport = new FakeTransport();
        const sender = createShadowSender({ transport });
        const sessionId = "s-park-transient";

        sender.resetSession(sessionId, "route_reopen");
        await waitFor(() => transport.resetCalls === 1);
        sender.resetSession(sessionId, "route_reopen");
        await waitFor(() => transport.resetCalls === 2);

        expect(sender.getStats(sessionId).parked).toBe(0);
    });

    it("strips only tag prefixes known from tagger state and exact ctx-search hint blocks", () => {
        useTempDataHome("shadow-denorm-");
        const db = openDatabase();
        const sessionId = "s-denorm";
        appendAutoSearchHintDecision(db, sessionId, {
            messageId: "m1",
            decision: "hint",
            text: "\n\n<ctx-search-hint>hint text</ctx-search-hint>",
        });
        const textPart = {
            type: "text",
            text: "§1§ user text\n\n<ctx-search-hint>hint text</ctx-search-hint>",
        };
        const toolPart = {
            type: "tool",
            callID: "call-1",
            state: { output: "§99§ §12§ real content" },
        };
        const untouched = { type: "text", text: "§12§ real user prefix" };
        const outputMessages: MessageLike[] = [
            {
                info: { id: "m1", role: "user", sessionID: sessionId },
                parts: [textPart, untouched],
            },
            { info: { id: "m2", role: "assistant" }, parts: [toolPart] },
        ];
        const normalizationTargets: TagNormalizationTarget[] = [
            { tagNumber: 1, message: outputMessages[0], part: textPart, field: "text" },
            {
                tagNumber: 99,
                message: outputMessages[1],
                part: toolPart,
                field: "tool_state_output",
            },
        ];

        const { ts_output, normalizations } = __shadowSenderTest.denormalizeShadowOutput({
            db,
            sessionId,
            outputMessages,
            normalizationTargets,
        });

        const clone = ts_output as MessageLike[];
        expect((clone[0].parts[0] as { text: string }).text).toBe("user text");
        expect((clone[0].parts[1] as { text: string }).text).toBe("§12§ real user prefix");
        expect((clone[1].parts[0] as { state: { output: string } }).state.output).toBe(
            "§12§ real content",
        );
        expect(normalizations.map((entry) => entry.kind)).toEqual([
            "tag_prefix",
            "tag_prefix",
            "ctx_search_hint",
        ]);
    });

    it("primes ordinals once, appends only newer rows, and detects deletion drift", async () => {
        const sessionId = "s-ord";
        let rows = [
            { id: "m1", timeCreated: 1, contributesOrdinal: true, hasValidInfo: true },
            { id: "summary", timeCreated: 2, contributesOrdinal: false, hasValidInfo: true },
            { id: "m2", timeCreated: 3, contributesOrdinal: true, hasValidInfo: true },
        ];
        const pageAnchors: Array<{ timeCreated: number; id: string } | null> = [];
        const unregister = setRawMessageProvider(sessionId, {
            readMessages() {
                throw new Error("incremental ordinal reads must not hydrate the full session");
            },
            readMessageOrdinalPage(after, limit) {
                pageAnchors.push(after);
                return rows
                    .filter(
                        (row) =>
                            !after ||
                            row.timeCreated > after.timeCreated ||
                            (row.timeCreated === after.timeCreated && row.id > after.id),
                    )
                    .slice(0, limit);
            },
            getStoredMessageCount() {
                return rows.length;
            },
        });
        try {
            const state = __shadowSenderTest.createSessionQueueState();
            const first = await __shadowSenderTest.resolveOrdinalsForShadow({
                sessionId,
                messages: [message(sessionId, "m2", "visible")],
                generation: state.shadowGeneration,
                memoGeneration: state.idOrdinalMemoGeneration,
                memo: state.idOrdinalMemo,
                memoAnchor: state.idOrdinalMemoAnchor,
                memoStoredCount: state.idOrdinalMemoStoredCount,
                memoCanonicalCount: state.idOrdinalMemoCanonicalCount,
            });
            expect(first).toEqual(
                expect.objectContaining({
                    ok: true,
                    annotatedInput: [expect.objectContaining({ absolute_ordinal: 2 })],
                }),
            );
            if (!first.ok) throw new Error("ordinal prime unexpectedly failed");

            rows = [
                ...rows,
                { id: "m3", timeCreated: 4, contributesOrdinal: true, hasValidInfo: true },
                { id: "m4", timeCreated: 5, contributesOrdinal: true, hasValidInfo: true },
                { id: "m5", timeCreated: 6, contributesOrdinal: true, hasValidInfo: true },
            ];
            const second = await __shadowSenderTest.resolveOrdinalsForShadow({
                sessionId,
                messages: [message(sessionId, "m5", "new")],
                generation: state.shadowGeneration,
                memoGeneration: first.memoGeneration,
                memo: state.idOrdinalMemo,
                memoAnchor: first.memoAnchor,
                memoStoredCount: first.memoStoredCount,
                memoCanonicalCount: first.memoCanonicalCount,
            });
            expect(second).toEqual(
                expect.objectContaining({
                    ok: true,
                    annotatedInput: [expect.objectContaining({ absolute_ordinal: 5 })],
                }),
            );
            expect(pageAnchors).toEqual([null, { timeCreated: 3, id: "m2" }]);
            if (!second.ok) throw new Error("incremental ordinal append unexpectedly failed");

            rows = rows.filter((row) => row.id !== "m1");
            expect(
                await __shadowSenderTest.resolveOrdinalsForShadow({
                    sessionId,
                    messages: [message(sessionId, "m5", "new")],
                    generation: state.shadowGeneration,
                    memoGeneration: second.memoGeneration,
                    memo: state.idOrdinalMemo,
                    memoAnchor: second.memoAnchor,
                    memoStoredCount: second.memoStoredCount,
                    memoCanonicalCount: second.memoCanonicalCount,
                }),
            ).toEqual(expect.objectContaining({ ok: false, reason: "mismatch" }));
        } finally {
            unregister();
        }
    });

    it("resets the shadow lineage when incremental count drift finds a deletion", async () => {
        useTempDataHome("shadow-ordinal-drift-reset-");
        const sessionId = "s-ordinal-drift-reset";
        const db = openDatabase();
        let rows = [
            { id: "m1", timeCreated: 1, contributesOrdinal: true, hasValidInfo: true },
            { id: "m2", timeCreated: 2, contributesOrdinal: true, hasValidInfo: true },
        ];
        const unregister = setRawMessageProvider(sessionId, {
            readMessages() {
                throw new Error("ordinal drift checks must not hydrate the full session");
            },
            readMessageOrdinalPage(after, limit) {
                return rows
                    .filter(
                        (row) =>
                            !after ||
                            row.timeCreated > after.timeCreated ||
                            (row.timeCreated === after.timeCreated && row.id > after.id),
                    )
                    .slice(0, limit);
            },
            getStoredMessageCount: () => rows.length,
        });
        try {
            const transport = new FakeTransport();
            const sender = createShadowSender({ transport });
            const tail = message(sessionId, "m2", "tail");
            sender.enqueue(
                basePass({ db, sessionId, inputMessages: [tail], outputMessages: [tail] }),
            );
            await waitFor(() => sender.getStats(sessionId).transforms_sent === 1);

            rows = rows.filter((row) => row.id !== "m1");
            sender.enqueue(
                basePass({
                    db,
                    sessionId,
                    inputMessages: [tail],
                    outputMessages: [tail],
                    nowMs: 2,
                }),
            );
            await waitFor(() => sender.getStats(sessionId).ordinal_mismatch === 1);

            expect(transport.calls.filter((call) => call.method === "shadow_reset")).toHaveLength(
                2,
            );
            expect(sender.getStats(sessionId).transforms_sent).toBe(2);
        } finally {
            unregister();
        }
    });

    it("self-heals a provisional ordinal when persistence agrees and resets once when it does not", async () => {
        const sessionId = "s-ordinal-provisional";
        let rows = [
            { id: "m1", timeCreated: 1, contributesOrdinal: true, hasValidInfo: true },
            { id: "m2", timeCreated: 2, contributesOrdinal: true, hasValidInfo: true },
        ];
        const unregister = setRawMessageProvider(sessionId, {
            readMessages() {
                throw new Error("ordinal tests must use incremental reads");
            },
            readMessageOrdinalPage(after, limit) {
                return rows
                    .filter(
                        (row) =>
                            !after ||
                            row.timeCreated > after.timeCreated ||
                            (row.timeCreated === after.timeCreated && row.id > after.id),
                    )
                    .slice(0, limit);
            },
            getStoredMessageCount: () => rows.length,
        });
        try {
            const state = __shadowSenderTest.createSessionQueueState();
            const provisional = [
                message(sessionId, "m1", "one"),
                message(sessionId, "m2", "two"),
                message(sessionId, "m3", "live tail"),
            ];
            const first = await __shadowSenderTest.resolveOrdinalsForShadow({
                sessionId,
                messages: provisional,
                generation: state.shadowGeneration,
                memoGeneration: state.idOrdinalMemoGeneration,
                memo: state.idOrdinalMemo,
                memoStoredCount: state.idOrdinalMemoStoredCount,
                memoAnchor: state.idOrdinalMemoAnchor,
                memoCanonicalCount: state.idOrdinalMemoCanonicalCount,
            });
            expect(first).toEqual(expect.objectContaining({ ok: true }));
            if (!first.ok) throw new Error("provisional ordinal setup failed");

            rows = [
                ...rows,
                { id: "m3", timeCreated: 3, contributesOrdinal: true, hasValidInfo: true },
            ];
            const healed = await __shadowSenderTest.resolveOrdinalsForShadow({
                sessionId,
                messages: provisional,
                generation: state.shadowGeneration,
                memoGeneration: first.memoGeneration,
                memo: state.idOrdinalMemo,
                memoStoredCount: first.memoStoredCount,
                memoAnchor: first.memoAnchor,
                memoCanonicalCount: first.memoCanonicalCount,
            });
            expect(healed).toEqual(
                expect.objectContaining({
                    ok: true,
                    annotatedInput: [
                        expect.objectContaining({ absolute_ordinal: 1 }),
                        expect.objectContaining({ absolute_ordinal: 2 }),
                        expect.objectContaining({ absolute_ordinal: 3 }),
                    ],
                }),
            );

            const resetTransport = new FakeTransport();
            const sender = createShadowSender({ transport: resetTransport });
            const db = openDatabase();
            rows = [
                { id: "m1", timeCreated: 1, contributesOrdinal: true, hasValidInfo: true },
                { id: "m2", timeCreated: 2, contributesOrdinal: true, hasValidInfo: true },
            ];
            sender.enqueue(
                basePass({
                    db,
                    sessionId,
                    inputMessages: provisional,
                    outputMessages: provisional,
                }),
            );
            await waitFor(() => sender.getStats(sessionId).transforms_sent === 1);
            rows = [
                ...rows,
                { id: "inserted", timeCreated: 2.5, contributesOrdinal: true, hasValidInfo: true },
                { id: "m3", timeCreated: 3, contributesOrdinal: true, hasValidInfo: true },
            ];
            sender.enqueue(
                basePass({
                    db,
                    sessionId,
                    inputMessages: provisional,
                    outputMessages: provisional,
                    nowMs: 2,
                }),
            );
            await waitFor(() => sender.getStats(sessionId).transforms_sent === 2);
            expect(
                resetTransport.calls.filter((call) => call.method === "shadow_reset"),
            ).toHaveLength(2);
            expect(sender.getStats(sessionId).ordinal_mismatch).toBe(1);
        } finally {
            unregister();
        }
    });

    it("sends compacted sessions after excluding summary rows from both comparison lanes", async () => {
        useTempDataHome("shadow-compacted-");
        const sessionId = "s-compacted";
        createOpenCodeDb(sessionId, [
            { id: "m1", role: "user", text: "before" },
            { id: "summary", role: "assistant", text: "summary", summary: true },
            { id: "m2", role: "assistant", text: "after" },
        ]);
        const db = openDatabase();
        const transport = new FakeTransport();
        const sender = createShadowSender({ transport });
        const inputMessages = [
            message(sessionId, "m1", "before"),
            summaryMessage(sessionId, "summary"),
            message(sessionId, "m2", "after"),
        ];

        sender.enqueue(
            basePass({
                db,
                sessionId,
                inputMessages,
                outputMessages: structuredClone(inputMessages),
            }),
        );
        await waitFor(
            () => transport.calls.filter((call) => call.method === "shadow_transform").length === 1,
        );

        const body = transport.calls.find((call) => call.method === "shadow_transform")?.body as {
            input: Array<{ info: { id: string }; absolute_ordinal: number }>;
            ts_output: Array<{ info: { id: string } }>;
            normalizations: Array<{ kind: string; message_id: string; field: string }>;
        };
        expect(body.input.map((entry) => [entry.info.id, entry.absolute_ordinal])).toEqual([
            ["m1", 1],
            ["m2", 2],
        ]);
        expect(body.ts_output.map((entry) => entry.info.id)).toEqual(["m1", "m2"]);
        expect(body.normalizations).toEqual(
            expect.arrayContaining([
                expect.objectContaining({
                    kind: "summary_message",
                    message_id: "summary",
                    field: "input",
                }),
                expect.objectContaining({
                    kind: "summary_message",
                    message_id: "summary",
                    field: "ts_output",
                }),
            ]),
        );
        expect(sender.getStats(sessionId).ordinal_unresolved).toBe(0);
        expect(sender.getStats(sessionId).transforms_sent).toBe(1);
    });

    it("coalesces pending state-sync work into the newest unsent pass", async () => {
        useTempDataHome("shadow-fifo-");
        const sessionId = "s-fifo";
        createOpenCodeDb(sessionId, [
            { id: "m1", role: "user", text: "one" },
            { id: "m2", role: "user", text: "two" },
            { id: "m3", role: "user", text: "three" },
        ]);
        const db = openDatabase();
        const transport = new FakeTransport();
        transport.blockFirstReset();
        const sender = createShadowSender({ transport });

        for (let index = 1; index <= 3; index += 1) {
            const msg = message(sessionId, `m${index}`, String(index));
            sender.enqueue(
                basePass({
                    db,
                    sessionId,
                    inputMessages: [msg],
                    outputMessages: [msg],
                    nowMs: index,
                }),
            );
        }
        expect(sender.getQueueDepth(sessionId)).toBe(1);
        expect(sender.getStats(sessionId).dropped_oldest).toBe(1);
        transport.releaseReset?.();
        await waitFor(
            () => transport.calls.filter((call) => call.method === "shadow_transform").length === 2,
        );

        // Wire bodies are FLAT (op fields beside `method`) — the module's serde
        // parsers reject a nested `params` object with invalid_params.
        const sentNowMs = transport.calls
            .filter((call) => call.method === "shadow_transform")
            .map((call) => (call.body as { pass_inputs: { now_ms: number } }).pass_inputs.now_ms);
        expect(sentNowMs).toEqual([1, 3]);
        expect(transport.calls.filter((call) => call.method === "state_sync")).toHaveLength(1);
        const firstTransformIndex = transport.calls.findIndex(
            (call) => call.method === "shadow_transform",
        );
        expect(transport.calls.slice(0, firstTransformIndex).map((call) => call.method)).toEqual([
            "shadow_reset",
            "state_sync",
        ]);
        const transformBodies = transport.calls
            .filter((call) => call.method === "shadow_transform")
            .map((call) => call.body as { seed_pass: boolean });
        expect(transformBodies[0].seed_pass).toBe(true);
        expect(transformBodies.slice(1).every((body) => body.seed_pass === false)).toBe(true);
    });

    it("keeps the per-session queue within its absolute bound and preserves the newest pass", async () => {
        useTempDataHome("shadow-queue-bound-");
        const sessionId = "s-queue-bound";
        createOpenCodeDb(
            sessionId,
            Array.from({ length: 8 }, (_, index) => ({
                id: `m${index + 1}`,
                role: "user",
                text: String(index + 1),
            })),
        );
        const db = openDatabase();
        const transport = new FakeTransport();
        transport.blockFirstReset();
        const sender = createShadowSender({ transport, queueMaxDepth: 1 });

        for (let index = 1; index <= 8; index += 1) {
            const msg = message(sessionId, `m${index}`, String(index));
            sender.enqueue(
                basePass({
                    db,
                    sessionId,
                    inputMessages: [msg],
                    outputMessages: [msg],
                    nowMs: index,
                }),
            );
        }

        expect(sender.getQueueDepth(sessionId)).toBe(1);
        expect(sender.getStats(sessionId).dropped_oldest).toBe(6);
        transport.releaseReset?.();
        await waitFor(
            () => transport.calls.filter((call) => call.method === "shadow_transform").length === 2,
        );
        const sentNowMs = transport.calls
            .filter((call) => call.method === "shadow_transform")
            .map((call) => (call.body as { pass_inputs: { now_ms: number } }).pass_inputs.now_ms);
        expect(sentNowMs).toEqual([1, 8]);
    });

    it("times out a stalled sync, releases its payload, and drains the bounded latest pass", async () => {
        useTempDataHome("shadow-stalled-sync-");
        const sessionId = "s-stalled-sync";
        createOpenCodeDb(sessionId, [
            { id: "m1", role: "user", text: "one" },
            { id: "m2", role: "user", text: "two" },
            { id: "m3", role: "user", text: "three" },
        ]);
        const db = openDatabase();
        const transport = new StallingSyncTransport();
        const sender = createShadowSender({ transport, sendTimeoutMs: 20, queueMaxDepth: 1 });
        const pass = (index: number) => {
            const msg = message(sessionId, `m${index}`, String(index));
            return basePass({
                db,
                sessionId,
                inputMessages: [msg],
                outputMessages: [msg],
                nowMs: index,
            });
        };

        sender.enqueue(pass(1));
        await waitFor(() => transport.retainedBody !== null);
        sender.enqueue(pass(2));
        sender.enqueue(pass(3));

        expect(sender.getQueueDepth(sessionId)).toBe(1);
        expect(sender.getStats(sessionId).dropped_oldest).toBe(1);
        await waitFor(() => sender.getStats(sessionId).transforms_sent === 1);

        expect(sender.getStats(sessionId).send_timeouts).toBe(1);
        expect(transport.retainedBody).toBeNull();
        expect(sender.getQueueDepth(sessionId)).toBe(0);
        expect(transport.methods).toEqual([
            "shadow_reset",
            "state_sync",
            "shadow_reset",
            "state_sync",
            "shadow_transform",
        ]);
        const compared = transport.transformBodies[0] as {
            pass_inputs: { now_ms: number };
            input: Array<{ info: { id: string } }>;
            ts_output: Array<{ info: { id: string } }>;
        };
        expect(compared.pass_inputs.now_ms).toBe(3);
        expect(compared.input.map((entry) => entry.info.id)).toEqual(["m3"]);
        expect(compared.ts_output.map((entry) => entry.info.id)).toEqual(["m3"]);
    });

    it("uses id-only and point readers across ordinal, declared-trim, and state-sync paths", async () => {
        useTempDataHome("shadow-point-reads-");
        const sessionId = "s-point-reads";
        const db = openDatabase();
        appendCompartments(db, sessionId, [
            {
                sequence: 0,
                startMessage: 1,
                endMessage: 2,
                startMessageId: "m1",
                endMessageId: "m2",
                title: "Point-read compartment",
                content: "content",
            },
        ]);
        setPersistedCompactionMarkerState(db, sessionId, {
            boundaryMessageId: "m2",
            summaryMessageId: "summary",
            compactionPartId: "compaction-part",
            summaryPartId: "summary-part",
            boundaryOrdinal: 2,
            targetEndMessageId: "m2",
        });
        const rawById = new Map([
            [
                "m1",
                {
                    ordinal: 1,
                    id: "m1",
                    role: "user",
                    parts: [{ type: "text", text: "one" }],
                    createdAt: new Date(2025, 0, 2, 12).getTime(),
                },
            ],
            [
                "m2",
                {
                    ordinal: 2,
                    id: "m2",
                    role: "assistant",
                    parts: [{ type: "text", text: "two" }],
                    createdAt: new Date(2025, 0, 3, 12).getTime(),
                },
            ],
        ]);
        let fullReads = 0;
        let pointReads = 0;
        const unregister = setRawMessageProvider(sessionId, {
            readMessages() {
                fullReads += 1;
                throw new Error("full raw-session reads are forbidden on shadow paths");
            },
            readMessageOrdinalPage(after) {
                if (after) return [];
                return [
                    {
                        id: "m1",
                        timeCreated: 1,
                        contributesOrdinal: true,
                        hasValidInfo: true,
                    },
                    {
                        id: "m2",
                        timeCreated: 2,
                        contributesOrdinal: true,
                        hasValidInfo: true,
                    },
                ];
            },
            getStoredMessageCount() {
                return 2;
            },
            readMessageById(messageId) {
                pointReads += 1;
                return rawById.get(messageId) ?? null;
            },
        });
        try {
            const state = __shadowSenderTest.createSessionQueueState();
            expect(
                await __shadowSenderTest.resolveOrdinalsForShadow({
                    sessionId,
                    messages: [message(sessionId, "m2", "two")],
                    generation: state.shadowGeneration,
                    memoGeneration: state.idOrdinalMemoGeneration,
                    memo: state.idOrdinalMemo,
                }),
            ).toEqual(expect.objectContaining({ ok: true }));
            expect(__shadowSenderTest.resolveDeclaredTrimForShadow({ db, sessionId })).toEqual(
                expect.objectContaining({ flat_boundary_id: "m2#0" }),
            );
            const sync = await __shadowSenderTest.buildStateSyncPayload({
                state,
                pass: basePass({ db, sessionId }),
                force: true,
            });
            expect(sync).toEqual(
                expect.objectContaining({
                    params: expect.objectContaining({
                        compartments: [
                            expect.objectContaining({ start_message: 1, end_message: 2 }),
                        ],
                    }),
                }),
            );
            expect(fullReads).toBe(0);
            expect(pointReads).toBe(3);
        } finally {
            unregister();
        }
    });

    it("aligns declared trim ordinals with the canonical basis after a summary row", () => {
        useTempDataHome("shadow-canonical-declared-trim-");
        const sessionId = "s-canonical-declared-trim";
        createOpenCodeDb(sessionId, [
            { id: "m1", role: "user", text: "before" },
            { id: "summary", role: "assistant", text: "summary", summary: true },
            { id: "m2", role: "user", text: "boundary" },
        ]);
        const db = openDatabase();
        appendCompartments(db, sessionId, [
            {
                sequence: 1,
                startMessage: 3,
                endMessage: 3,
                startMessageId: "m2",
                endMessageId: "m2",
                title: "Boundary",
                content: "content",
            },
        ]);
        setPersistedCompactionMarkerState(db, sessionId, {
            boundaryMessageId: "m2",
            summaryMessageId: "summary",
            compactionPartId: "compaction-part",
            summaryPartId: "summary-part",
            boundaryOrdinal: 3,
            targetEndMessageId: "m2",
        });

        expect(__shadowSenderTest.resolveDeclaredTrimForShadow({ db, sessionId })).toEqual(
            expect.objectContaining({
                boundary_absolute_ordinal: 2,
                next_absolute_ordinal: 3,
            }),
        );
    });

    it("seeds ordered active user-profile lines without reading project memories", async () => {
        useTempDataHome("shadow-profile-seed-");
        const db = openDatabase();
        insertUserMemory(db, "prefers root cause", []);
        insertUserMemory(db, "x < y & z", []);
        const state = __shadowSenderTest.createSessionQueueState();
        const sync = await __shadowSenderTest.buildStateSyncPayload({
            state,
            pass: basePass({ db, sessionId: "s-profile-seed" }),
            force: true,
        });
        expect(sync).toEqual(
            expect.objectContaining({
                params: expect.objectContaining({
                    user_profile: ["prefers root cause", "x < y & z"],
                }),
            }),
        );
    });

    it("keeps active profile lines in the real multi-batch seed wire shape", () => {
        const profile = ["prefers root cause", "x < y & z"];
        const batches = __shadowSenderTest.buildPagedSeedPayloads({
            shadowGeneration: 7,
            expectedShadowSeq: 11,
            seedId: "profile-paged-seed",
            seedBoundaryId: "m2#0",
            compartments: [
                { sequence: 0, content: "a".repeat(300 * 1024) },
                { sequence: 1, content: "b".repeat(300 * 1024) },
            ],
            memories: [],
            memoryMutations: [],
            userProfile: profile,
            workspace: null,
            lastTodoState: "[]",
            watermarks: {
                compartment_sequence: 1,
                memory_id: 0,
                m0_mutation_id: 0,
                memory_mutation_id: 0,
                last_todo_state_hash: "hash",
            },
        });

        expect(batches.length).toBeGreaterThan(1);
        expect(batches.flatMap((batch) => batch.params.user_profile)).toEqual(profile);
        expect(
            batches.flatMap((batch) => {
                const wire = __shadowSenderTest.toFlatWireBody(batch) as Record<string, unknown>;
                return (wire.user_profile as string[]) ?? [];
            }),
        ).toEqual(profile);
        expect(batches.at(-1)?.params.seed_complete).toBe(true);
    });

    it("pages force seeds by exact flat wire bytes and keeps scalar state on the final batch", () => {
        const watermarks = {
            compartment_sequence: 2,
            memory_id: 0,
            m0_mutation_id: 0,
            memory_mutation_id: 0,
            last_todo_state_hash: "hash",
        };
        const compartments = ["a", "b", "c"].map((label, sequence) => ({
            sequence,
            content: `${label}${"x".repeat(300 * 1024)}`,
        }));
        const batches = __shadowSenderTest.buildPagedSeedPayloads({
            shadowGeneration: 7,
            expectedShadowSeq: 11,
            seedId: "exact-size-seed",
            seedBoundaryId: "m2#0",
            compartments,
            memories: [],
            memoryMutations: [],
            userProfile: [],
            workspace: null,
            lastTodoState: "[]",
            watermarks,
        });

        expect(batches.length).toBeGreaterThan(1);
        expect(
            batches.every(
                (batch) =>
                    __shadowSenderTest.flatWireBodyBytes(batch) <=
                    __shadowSenderTest.SHADOW_SEED_BATCH_MAX_BYTES,
            ),
        ).toBe(true);
        expect(
            batches
                .flatMap((batch) => batch.params.compartments)
                .map((item) => (item as { sequence: number }).sequence),
        ).toEqual([0, 1, 2]);
        for (const [index, batch] of batches.entries()) {
            expect(batch.params.seed_batch_index).toBe(index);
            expect(batch.params.seed_batch_total).toBe(batches.length);
            expect(batch.params.seed_id).toBe("exact-size-seed");
            expect(batch.params.seed_generation).toBe(7);
            expect(batch.params.expected_shadow_seq).toBe(11);
            if (index + 1 < batches.length) {
                expect(batch.params.seed_complete).toBe(false);
                expect(batch.params.seed_boundary_id).toBeUndefined();
                expect(batch.params.workspace).toBeUndefined();
                expect(batch.params.last_todo_state).toBeUndefined();
                expect(batch.params.acked_watermarks).toBeUndefined();
            }
        }
        expect(batches.at(-1)?.params).toEqual(
            expect.objectContaining({
                seed_complete: true,
                seed_boundary_id: "m2#0",
                workspace: null,
                last_todo_state: "[]",
                acked_watermarks: watermarks,
            }),
        );

        expect(() =>
            __shadowSenderTest.buildPagedSeedPayloads({
                shadowGeneration: 1,
                expectedShadowSeq: 0,
                seedId: "oversized-item",
                seedBoundaryId: null,
                compartments: [{ content: "x".repeat(513 * 1024) }],
                memories: [],
                memoryMutations: [],
                userProfile: [],
                workspace: null,
                lastTodoState: "[]",
                watermarks,
            }),
        ).toThrow("512 KiB");
        const splitTail = __shadowSenderTest.buildPagedSeedPayloads({
            shadowGeneration: 1,
            expectedShadowSeq: 0,
            seedId: "split-tail",
            seedBoundaryId: null,
            compartments: [{ content: "x".repeat(300 * 1024) }],
            memories: [],
            memoryMutations: [],
            userProfile: [],
            workspace: null,
            lastTodoState: "y".repeat(300 * 1024),
            watermarks,
        });
        expect(splitTail).toHaveLength(2);
        expect(splitTail[0]?.params.compartments).toHaveLength(1);
        expect(splitTail[0]?.params.seed_complete).toBe(false);
        expect(splitTail[1]?.params.compartments).toHaveLength(0);
        expect(splitTail[1]?.params.seed_complete).toBe(true);
        expect(
            splitTail.every((batch) => __shadowSenderTest.flatWireBodyBytes(batch) <= 512 * 1024),
        ).toBe(true);

        expect(() =>
            __shadowSenderTest.buildPagedSeedPayloads({
                shadowGeneration: 1,
                expectedShadowSeq: 0,
                seedId: "oversized-tail",
                seedBoundaryId: null,
                compartments: [],
                memories: [],
                memoryMutations: [],
                userProfile: [],
                workspace: null,
                lastTodoState: "x".repeat(513 * 1024),
                watermarks,
            }),
        ).toThrow("512 KiB");
    });

    it("pages large transform requests and reassembles every array in order", () => {
        const original: Record<string, unknown> = {
            method: "shadow_transform",
            session_id: "shadow:large",
            shadow_generation: 3,
            seed_pass: false,
            input: ["a".repeat(260 * 1024), "b".repeat(260 * 1024)],
            ts_output: ["out-a".repeat(60 * 1024), "out-b".repeat(60 * 1024)],
            normalizations: [{ kind: "summary_message", message_id: "s" }],
            pass_inputs: { now_ms: 1 },
            ts_decision: { class: "defer" },
            declared_trim: null,
        };
        const pages = __shadowSenderTest.buildPagedTransformPayloads(original);
        expect(pages.length).toBeGreaterThan(1);
        expect(
            pages.every(
                (page) =>
                    Buffer.byteLength(JSON.stringify(page)) <=
                    __shadowSenderTest.SHADOW_TRANSFORM_PAGE_MAX_BYTES,
            ),
        ).toBe(true);
        const assembled = { ...pages.at(-1) } as Record<string, unknown>;
        for (const field of [
            "transform_page_id",
            "transform_generation",
            "transform_page_index",
            "transform_page_total",
            "transform_page_complete",
            "transform_page_digest",
        ]) {
            delete assembled[field];
        }
        for (const field of ["input", "ts_output", "normalizations"]) {
            assembled[field] = pages.flatMap((page) => (page[field] as unknown[]) ?? []);
        }
        expect(assembled).toEqual(original);
    });

    it("slices one oversized transform item and reassembles its original JSON", () => {
        const item = { id: "oversized", text: "x".repeat(2 * 1024 * 1024) };
        const body: Record<string, unknown> = {
            method: "shadow_transform",
            session_id: "shadow:oversized",
            shadow_generation: 3,
            seed_pass: false,
            input: [item],
            ts_output: [],
            normalizations: [],
            pass_inputs: { now_ms: 1 },
            ts_decision: { class: "defer" },
            declared_trim: null,
        };
        const pages = __shadowSenderTest.buildPagedTransformPayloads(body);
        expect(pages.length).toBeGreaterThan(1);
        expect(
            pages.every(
                (page) =>
                    Buffer.byteLength(JSON.stringify(page)) <=
                    __shadowSenderTest.SHADOW_TRANSFORM_PAGE_MAX_BYTES,
            ),
        ).toBe(true);
        const chunks = pages.flatMap((page) =>
            ((page.input as Array<Record<string, unknown>> | undefined) ?? []).filter(
                (value) => value.__shadow_item_continuation !== undefined,
            ),
        );
        expect(chunks.length).toBeGreaterThan(1);
        const reassembled = JSON.parse(chunks.map((chunk) => chunk.chunk as string).join(""));
        expect(reassembled).toEqual(item);
    });

    it("does not interleave pages from two transform passes for one session", async () => {
        useTempDataHome("shadow-transform-pages-");
        const sessionId = "s-transform-pages";
        createOpenCodeDb(sessionId, [
            { id: "m1", role: "user", text: "one" },
            { id: "m2", role: "user", text: "two" },
            { id: "m3", role: "user", text: "three" },
        ]);
        const db = openDatabase();
        const { unregister } = installLinearRawProvider(sessionId, 3);
        try {
            const transport = new FakeTransport();
            transport.blockFirstTransformPage();
            const sender = createShadowSender({ transport });
            const large = [
                message(sessionId, "m1", "a".repeat(220 * 1024)),
                message(sessionId, "m2", "b".repeat(220 * 1024)),
                message(sessionId, "m3", "c".repeat(220 * 1024)),
            ];
            sender.enqueue(
                basePass({
                    db,
                    sessionId,
                    inputMessages: large,
                    outputMessages: structuredClone(large),
                }),
            );
            await waitFor(() =>
                transport.calls.some(
                    (call) =>
                        call.method === "shadow_transform" &&
                        (call.body as { transform_page_index?: number }).transform_page_index === 0,
                ),
            );
            sender.enqueue(
                basePass({
                    db,
                    sessionId,
                    inputMessages: large,
                    outputMessages: structuredClone(large),
                    nowMs: 2,
                }),
            );
            expect(sender.getQueueDepth(sessionId)).toBe(1);
            transport.releaseTransform?.();
            await waitFor(() => sender.getStats(sessionId).transforms_sent === 2);
            const pageCalls = transport.calls.filter((call) => call.method === "shadow_transform");
            const pageIds = pageCalls.map(
                (call) => (call.body as { transform_page_id?: string }).transform_page_id,
            );
            const firstId = pageIds[0];
            const firstEnd = pageIds.lastIndexOf(firstId);
            expect(firstId).toBeDefined();
            expect(pageIds.slice(0, firstEnd + 1).every((id) => id === firstId)).toBe(true);
            expect(pageIds.slice(firstEnd + 1).every((id) => id !== firstId)).toBe(true);
        } finally {
            unregister();
        }
    });

    it("derives the seed boundary from the serialized tail compartment snapshot", async () => {
        useTempDataHome("shadow-seed-boundary-snapshot-");
        const sessionId = "s-seed-boundary-snapshot";
        const db = openDatabase();
        appendCompartments(db, sessionId, [
            {
                sequence: 2,
                startMessage: 2,
                endMessage: 2,
                startMessageId: "m2",
                endMessageId: "m2",
                title: "new tail",
                content: "new",
            },
            {
                sequence: 1,
                startMessage: 1,
                endMessage: 1,
                startMessageId: "m1",
                endMessageId: "m1",
                title: "older",
                content: "old",
            },
        ]);
        setPersistedCompactionMarkerState(db, sessionId, {
            boundaryMessageId: "m1",
            summaryMessageId: "summary",
            compactionPartId: "compaction-part",
            summaryPartId: "summary-part",
            boundaryOrdinal: 1,
            targetEndMessageId: "m1",
        });
        const unregister = setRawMessageProvider(sessionId, {
            readMessages() {
                throw new Error("seed boundary derivation must use point reads");
            },
            readMessageOrdinalPage() {
                return [
                    { id: "m1", timeCreated: 1, contributesOrdinal: true, hasValidInfo: true },
                    { id: "m2", timeCreated: 2, contributesOrdinal: true, hasValidInfo: true },
                ];
            },
            getStoredMessageCount: () => 2,
            readMessagePartsById(messageId) {
                return {
                    id: messageId,
                    role: "user",
                    parts: [{ type: "text", text: messageId }],
                    createdAt: messageId === "m1" ? 1 : 2,
                };
            },
        });
        try {
            const state = __shadowSenderTest.createSessionQueueState();
            state.seedPassPending = true;
            const sync = await __shadowSenderTest.buildStateSyncPayload({
                state,
                pass: basePass({ db, sessionId }),
                force: true,
            });
            expect(sync).toEqual(
                expect.objectContaining({
                    params: expect.objectContaining({ seed_boundary_id: "m2#0" }),
                }),
            );
        } finally {
            unregister();
        }
    });

    it("sends paged seed batches sequentially and closes failed attempts before retry", async () => {
        useTempDataHome("shadow-paged-send-");
        const sessionId = "s-paged-send";
        const db = openDatabase();
        appendCompartments(
            db,
            sessionId,
            Array.from({ length: 3 }, (_, sequence) => ({
                sequence,
                startMessage: sequence + 1,
                endMessage: sequence + 1,
                startMessageId: `m${sequence + 1}`,
                endMessageId: `m${sequence + 1}`,
                title: `Large ${sequence}`,
                content: `${sequence}${"x".repeat(300 * 1024)}`,
            })),
        );
        const { unregister } = installLinearRawProvider(sessionId, 3);
        try {
            const transport = new FakeTransport();
            transport.failSyncAtCall = 2;
            const sender = createShadowSender({ transport });
            const tail = message(sessionId, "m3", "tail");
            const pass = basePass({ db, sessionId, inputMessages: [tail], outputMessages: [tail] });

            sender.enqueue(pass);
            await waitFor(() => sender.getStats(sessionId).send_failures === 1);
            expect(sender.getStats(sessionId).transforms_sent).toBe(0);
            expect(transport.closedSessions).toContain(sessionId);
            const firstAttemptBodies = transport.calls
                .filter((call) => call.method === "state_sync")
                .map((call) => call.body as { seed_id: string; seed_complete: boolean });
            expect(firstAttemptBodies.length).toBe(2);
            expect(firstAttemptBodies[0].seed_complete).toBe(false);
            expect(firstAttemptBodies[1].seed_id).toBe(firstAttemptBodies[0].seed_id);

            sender.enqueue({ ...pass, passInputs: { ...pass.passInputs, now_ms: 2 } });
            await waitFor(() => sender.getStats(sessionId).transforms_sent === 1);
            const allSeedBodies = transport.calls
                .filter((call) => call.method === "state_sync")
                .map(
                    (call) =>
                        call.body as {
                            seed_id: string;
                            seed_batch_index: number;
                            seed_batch_total: number;
                            seed_complete: boolean;
                            acked_watermarks?: unknown;
                        },
                );
            const seedIds = [...new Set(allSeedBodies.map((body) => body.seed_id))];
            expect(seedIds).toHaveLength(2);
            const retry = allSeedBodies.filter((body) => body.seed_id === seedIds[1]);
            expect(retry.map((body) => body.seed_batch_index)).toEqual(
                Array.from({ length: retry.length }, (_, index) => index),
            );
            expect(retry.every((body) => body.seed_batch_total === retry.length)).toBe(true);
            expect(retry.slice(0, -1).every((body) => body.acked_watermarks === undefined)).toBe(
                true,
            );
            expect(retry.at(-1)?.seed_complete).toBe(true);
            expect(retry.at(-1)?.acked_watermarks).toEqual(expect.any(Object));
            expect(
                allSeedBodies.every(
                    (body) =>
                        Buffer.byteLength(JSON.stringify(body)) <=
                        __shadowSenderTest.SHADOW_SEED_BATCH_MAX_BYTES,
                ),
            ).toBe(true);
        } finally {
            unregister();
        }
    });

    it("keeps the seed budget active through every batch acknowledgement", async () => {
        useTempDataHome("shadow-paged-budget-");
        const sessionId = "s-paged-budget";
        const db = openDatabase();
        appendCompartments(
            db,
            sessionId,
            Array.from({ length: 2 }, (_, sequence) => ({
                sequence,
                startMessage: sequence + 1,
                endMessage: sequence + 1,
                startMessageId: `m${sequence + 1}`,
                endMessageId: `m${sequence + 1}`,
                title: `Budget ${sequence}`,
                content: "x".repeat(300 * 1024),
            })),
        );
        const { unregister } = installLinearRawProvider(sessionId, 2);
        let clock = 0;
        let syncCalls = 0;
        const closed: string[] = [];
        const transport: ShadowTransport = {
            async call(req) {
                if (req.method === "shadow_reset") {
                    return { result: { shadow_generation: 1, shadow_seq: 0 } };
                }
                if (req.method === "state_sync") {
                    syncCalls += 1;
                    clock += 6;
                    const body = req.body as { seed_complete?: boolean };
                    return body.seed_complete
                        ? { result: { shadow_seq: 1 } }
                        : { result: { staged: true } };
                }
                return { result: { shadow_seq: 2, quarantined: false } };
            },
            closeSession(id) {
                closed.push(id);
            },
        };
        try {
            const sender = createShadowSender({
                transport,
                seedBudgetMs: 10,
                seedClock: () => clock,
            });
            const tail = message(sessionId, "m2", "tail");
            sender.enqueue(
                basePass({ db, sessionId, inputMessages: [tail], outputMessages: [tail] }),
            );
            await waitFor(() => sender.getStats(sessionId).seed_budget_exceeded === 1);

            expect(syncCalls).toBe(2);
            expect(sender.getStats(sessionId).transforms_sent).toBe(0);
            expect(closed).toContain(sessionId);
        } finally {
            unregister();
        }
    });

    it("resets and rebuilds an oversized legacy delta as a paged seed", async () => {
        useTempDataHome("shadow-oversized-delta-");
        const sessionId = "s-oversized-delta";
        createOpenCodeDb(sessionId, [{ id: "m1", role: "user", text: "tail" }]);
        const db = openDatabase();
        const transport = new FakeTransport();
        const sender = createShadowSender({ transport });
        const tail = message(sessionId, "m1", "tail");
        const firstPass = basePass({
            db,
            sessionId,
            inputMessages: [tail],
            outputMessages: [tail],
        });
        sender.enqueue(firstPass);
        await waitFor(() => sender.getStats(sessionId).transforms_sent === 1);

        appendCompartments(
            db,
            sessionId,
            Array.from({ length: 4 }, (_, sequence) => ({
                sequence,
                startMessage: 1,
                endMessage: 1,
                startMessageId: "m1",
                endMessageId: "m1",
                title: `Oversized ${sequence}`,
                content: `${sequence}${"x".repeat(300 * 1024)}`,
            })),
        );
        sender.enqueue({ ...firstPass, passInputs: { ...firstPass.passInputs, now_ms: 2 } });
        await waitFor(() => sender.getStats(sessionId).transforms_sent === 2);

        const resetReasons = transport.calls
            .filter((call) => call.method === "shadow_reset")
            .map((call) => (call.body as { reason: string }).reason);
        expect(resetReasons).toEqual(["cold_start", "oversized_state_sync"]);
        const secondSeedBodies = transport.calls
            .slice(transport.calls.findLastIndex((call) => call.method === "shadow_reset") + 1)
            .filter((call) => call.method === "state_sync")
            .map((call) => call.body as { seed_id: string; seed_batch_total: number });
        expect(secondSeedBodies.length).toBeGreaterThan(1);
        expect(
            secondSeedBodies.every((body) => body.seed_batch_total === secondSeedBodies.length),
        ).toBe(true);
        expect(new Set(secondSeedBodies.map((body) => body.seed_id)).size).toBe(1);
    });

    it("seeds 200 compartments with one ordinal prime and parts-only point reads", async () => {
        useTempDataHome("shadow-seed-cost-");
        const sessionId = "s-seed-cost";
        const db = openDatabase();
        appendLinearCompartments(db, sessionId, 200);
        const { counters, unregister } = installLinearRawProvider(sessionId, 200);
        try {
            const transport = new FakeTransport();
            const sender = createShadowSender({ transport });
            const tail = message(sessionId, "m200", "tail");
            sender.enqueue(
                basePass({ db, sessionId, inputMessages: [tail], outputMessages: [tail] }),
            );
            await waitFor(() => sender.getStats(sessionId).transforms_sent === 1);

            expect(counters.fullReads).toBe(0);
            expect(counters.ordinalPageReads).toBe(1);
            expect(counters.storedCounts).toBe(1);
            expect(counters.partReads).toBeLessThanOrEqual(200);
        } finally {
            unregister();
        }
    });

    it("yields often enough for interval timers to run during a large seed", async () => {
        useTempDataHome("shadow-seed-yield-");
        const sessionId = "s-seed-yield";
        const db = openDatabase();
        appendLinearCompartments(db, sessionId, 100);
        const { unregister } = installLinearRawProvider(sessionId, 100);
        let previousTick = 0;
        let maxGap = 0;
        let timerTicks = 0;
        let serializationStarted = false;
        let serializedCount = 0;
        let resolveSerializationYielded: (() => void) | null = null;
        const serializationYielded = new Promise<void>((resolve) => {
            resolveSerializationYielded = resolve;
        });
        const timer = setInterval(() => {
            if (!serializationStarted) return;
            const tick = performance.now();
            timerTicks += 1;
            maxGap = Math.max(maxGap, tick - previousTick);
            previousTick = tick;
        }, 10);
        try {
            const transport = new FakeTransport();
            const sender = createShadowSender({
                transport,
                beforeSerializeCompartment() {
                    if (!serializationStarted) {
                        serializationStarted = true;
                        previousTick = performance.now();
                    }
                    serializedCount += 1;
                    if (serializedCount === 100)
                        setImmediate(() => resolveSerializationYielded?.());
                    const end = performance.now() + 2;
                    while (performance.now() < end) {
                        // Simulate non-trivial synchronous compartment serialization.
                    }
                },
            });
            const tail = message(sessionId, "m100", "tail");
            sender.enqueue(
                basePass({ db, sessionId, inputMessages: [tail], outputMessages: [tail] }),
            );
            await serializationYielded;
            maxGap = Math.max(maxGap, performance.now() - previousTick);
            expect(serializedCount).toBe(100);
            expect(timerTicks).toBeGreaterThanOrEqual(5);
            expect(maxGap).toBeLessThan(750);
            await waitFor(() => sender.getStats(sessionId).transforms_sent === 1);
        } finally {
            clearInterval(timer);
            unregister();
        }
    });

    it("permanently skips a session when seed serialization exceeds its budget", async () => {
        useTempDataHome("shadow-seed-budget-");
        const sessionId = "s-seed-budget";
        const db = openDatabase();
        appendLinearCompartments(db, sessionId, 1);
        const { unregister } = installLinearRawProvider(sessionId, 1);
        let clock = 0;
        const budgetLogs: string[] = [];
        try {
            const transport = new FakeTransport();
            const sender = createShadowSender({
                transport,
                seedBudgetMs: 30,
                seedClock: () => clock,
                beforeSerializeCompartment: () => {
                    clock = 31;
                },
                onSeedBudgetExceeded: (line) => budgetLogs.push(line),
            });
            sender.enqueue(basePass({ db, sessionId }));
            await waitFor(() => sender.getStats(sessionId).seed_budget_exceeded === 1);

            expect(sender.getStats(sessionId).transforms_sent).toBe(0);
            expect(transport.calls.map((call) => call.method)).toEqual(["shadow_reset"]);
            expect(budgetLogs).toEqual(["shadow: seed budget exceeded, lane disabled for session"]);

            sender.enqueue(basePass({ db, sessionId, nowMs: 2 }));
            await new Promise((resolve) => setTimeout(resolve, 20));
            expect(transport.calls.map((call) => call.method)).toEqual(["shadow_reset"]);
            expect(sender.getStats(sessionId).seed_budget_exceeded).toBe(1);
        } finally {
            unregister();
        }
    });

    it("classifies a shared-client socket timeout and reopens the route without burning reseed allowance", async () => {
        useTempDataHome("shadow-reseed-read-timeout-");
        const sessionId = "s-reseed-read-timeout";
        createOpenCodeDb(sessionId, [{ id: "m1", role: "user", text: "one" }]);
        const db = openDatabase();

        const transport = new FakeTransport();
        transport.resetTimeoutError = new SocketTimeoutError("subc request timed out");
        transport.quarantinedResponsesRemaining = 2;
        transport.resetTimeoutAtCall = 2;
        const sender = createShadowSender({
            transport,
            reseedAttemptCap: 1,
            reseedCooldownMs: 60_000,
        });
        const msg = message(sessionId, "m1", "one");
        sender.enqueue(
            basePass({ db, sessionId, inputMessages: [msg], outputMessages: [msg], nowMs: 1 }),
        );
        await waitFor(() => sender.getStats(sessionId).connection_skips === 1);

        sender.enqueue(
            basePass({ db, sessionId, inputMessages: [msg], outputMessages: [msg], nowMs: 2 }),
        );
        await waitFor(
            () => transport.calls.filter((call) => call.method === "shadow_transform").length === 3,
        );

        const resetReasons = transport.calls
            .filter((call) => call.method === "shadow_reset")
            .map((call) => (call.body as { reason?: string }).reason);
        expect(resetReasons).toEqual([
            "cold_start",
            "quarantine_reseed",
            "route_reopen",
            "quarantine_reseed",
        ]);
        expect(sender.getStats(sessionId).connection_skips).toBe(1);
    });

    it("performs at most one reset and reseed retry after quarantine", async () => {
        useTempDataHome("shadow-quarantine-retry-");
        const sessionId = "s-quarantine-retry";
        createOpenCodeDb(sessionId, [{ id: "m1", role: "user", text: "one" }]);
        const db = openDatabase();
        const transport = new FakeTransport();
        transport.quarantinedResponsesRemaining = 2;
        const sender = createShadowSender({ transport });
        const msg = message(sessionId, "m1", "one");

        sender.enqueue(
            basePass({ db, sessionId, inputMessages: [msg], outputMessages: [msg], nowMs: 1 }),
        );
        await waitFor(
            () => transport.calls.filter((call) => call.method === "shadow_transform").length === 2,
        );

        expect(transport.calls.map((call) => call.method)).toEqual([
            "shadow_reset",
            "state_sync",
            "shadow_transform",
            "shadow_reset",
            "state_sync",
            "shadow_transform",
        ]);
        expect(
            transport.calls
                .filter((call) => call.method === "shadow_transform")
                .map((call) => (call.body as { seed_pass: boolean }).seed_pass),
        ).toEqual([true, true]);
        expect(sender.getStats(sessionId).resets_sent).toBe(2);
    });

    it("resets and reseeds once when the peer rejects a stale seed boundary", async () => {
        useTempDataHome("shadow-seed-boundary-retry-");
        const sessionId = "s-seed-boundary-retry";
        createOpenCodeDb(sessionId, [
            { id: "m1", role: "user", text: "covered" },
            { id: "m2", role: "assistant", text: "boundary" },
            { id: "m3", role: "user", text: "tail" },
        ]);
        const db = openDatabase();
        appendCompartments(db, sessionId, [
            {
                sequence: 1,
                startMessage: 1,
                endMessage: 2,
                startMessageId: "m1",
                endMessageId: "m2",
                title: "Seeded compartment",
                content: "covered summary",
            },
        ]);
        setPersistedCompactionMarkerState(db, sessionId, {
            boundaryMessageId: "m2",
            summaryMessageId: "summary",
            compactionPartId: "compaction-part",
            summaryPartId: "summary-part",
            boundaryOrdinal: 2,
            targetEndMessageId: "m2",
        });
        const transport = new FakeTransport();
        transport.seedBoundaryFailuresRemaining = 1;
        const sender = createShadowSender({ transport });
        const tail = message(sessionId, "m3", "tail");

        sender.enqueue(
            basePass({ db, sessionId, inputMessages: [tail], outputMessages: [tail], nowMs: 3 }),
        );
        await waitFor(
            () => transport.calls.filter((call) => call.method === "shadow_transform").length === 1,
        );

        expect(transport.calls.map((call) => call.method)).toEqual([
            "shadow_reset",
            "state_sync",
            "shadow_reset",
            "state_sync",
            "shadow_transform",
        ]);
        const syncBodies = transport.calls
            .filter((call) => call.method === "state_sync")
            .map((call) => call.body as { seed_boundary_id: string | null });
        expect(syncBodies.map((body) => body.seed_boundary_id)).toEqual(["m2#0", "m2#0"]);
        expect(sender.getStats(sessionId).resets_sent).toBe(2);
        expect(sender.getStats(sessionId).transforms_sent).toBe(1);
    });

    it("parks deterministic reseeding after the same reset reason repeats", async () => {
        useTempDataHome("shadow-reseed-cooldown-");
        const sessionId = "s-reseed-cooldown";
        createOpenCodeDb(sessionId, [{ id: "m1", role: "user", text: "tail" }]);
        const db = openDatabase();
        const transport = new FakeTransport();
        transport.seedBoundaryFailuresRemaining = 10;
        let now = 1_000;
        const sender = createShadowSender({
            transport,
            now: () => now,
            reseedCooldownMs: 100,
        });
        const pass = basePass({ db, sessionId });

        sender.enqueue(pass);
        await waitFor(() => sender.getStats(sessionId).send_failures === 1);
        expect(transport.calls.filter((call) => call.method === "shadow_reset")).toHaveLength(2);

        sender.enqueue({ ...pass, passInputs: { ...pass.passInputs, now_ms: 2 } });
        await waitFor(() => sender.getStats(sessionId).send_failures === 2);
        expect(transport.calls.filter((call) => call.method === "shadow_reset")).toHaveLength(3);

        now += 100;
        transport.seedBoundaryFailuresRemaining = 1;
        sender.enqueue({ ...pass, passInputs: { ...pass.passInputs, now_ms: 3 } });
        await waitFor(() => sender.getStats(sessionId).parked === 1);
        expect(sender.getStats(sessionId).transforms_sent).toBe(0);
        expect(transport.calls.filter((call) => call.method === "shadow_reset")).toHaveLength(3);

        sender.enqueue({ ...pass, passInputs: { ...pass.passInputs, now_ms: 4 } });
        expect(transport.calls.filter((call) => call.method === "shadow_reset")).toHaveLength(3);
    });

    it("serializes state_sync and shadow_transform with the shadow wire field inventory", async () => {
        useTempDataHome("shadow-wire-");
        const sessionId = "s-wire";
        createOpenCodeDb(sessionId, [
            { id: "m1", role: "user", text: "one" },
            { id: "m2", role: "assistant", text: "two" },
        ]);
        const db = openDatabase();
        appendCompartments(db, sessionId, [
            {
                sequence: 1,
                startMessage: 1,
                endMessage: 2,
                startMessageId: "m1",
                endMessageId: "m2",
                title: "Populated compartment",
                content: "full content",
                p1: "tier one",
                p2: "tier two",
                p3: "tier three",
                p4: "tier four",
                importance: 73,
                episodeType: "feature",
            },
        ]);
        const state = __shadowSenderTest.createSessionQueueState();
        state.lastAckedWatermarks = {
            compartment_sequence: 99,
            memory_id: 99,
            m0_mutation_id: 0,
            memory_mutation_id: 99,
            last_todo_state_hash: "already-acked",
        };
        const pass = basePass({ db, sessionId });

        const stateSync = await __shadowSenderTest.buildStateSyncPayload({
            state,
            pass,
            force: true,
        });
        expect(stateSync).toEqual(
            expect.objectContaining({
                method: "state_sync",
                params: expect.objectContaining({
                    shadow_generation: expect.any(Number),
                    expected_shadow_seq: expect.any(Number),
                    compartments: expect.any(Array),
                    memories: expect.any(Array),
                    memory_mutations: expect.any(Array),
                    last_todo_state: expect.any(String),
                }),
            }),
        );

        const transformBody = __shadowSenderTest.buildShadowTransformBody({
            state,
            pass: {
                ...pass,
                annotatedInput: [{ ...pass.inputMessages[0], absolute_ordinal: 1 }],
                declaredTrim: null,
            },
        });
        expect(transformBody).toEqual(
            expect.objectContaining({
                method: "shadow_transform",
                params: expect.objectContaining({
                    shadow_generation: expect.any(Number),
                    seed_pass: false,
                    input: expect.any(Array),
                    ts_output: expect.any(Array),
                    normalizations: expect.any(Array),
                    pass_inputs: expect.objectContaining({
                        now_ms: expect.any(Number),
                        model_key: expect.any(String),
                        usage: expect.objectContaining({
                            input_tokens: expect.any(Number),
                            limit: expect.any(Number),
                        }),
                        effective_execute_threshold: expect.any(Number),
                        history_budget_tokens: 19_500,
                        cache_ttl: expect.any(String),
                    }),
                    ts_decision: expect.objectContaining({ class: expect.any(String) }),
                    declared_trim: null,
                }),
            }),
        );

        // The on-wire form is FLAT: the module's serde parsers (ShadowStateSyncWire /
        // ShadowTransformWire) deserialize the whole request value, so op fields sit
        // beside `method` and a nested `params` key must NOT survive serialization.
        // This is the exact mismatch that made every live state_sync/shadow_transform
        // reject with invalid_params: missing field `shadow_generation`.
        const flatSync = __shadowSenderTest.toFlatWireBody(
            stateSync as { method: string; params: Record<string, unknown> },
        ) as Record<string, unknown>;
        expect(flatSync.method).toBe("state_sync");
        expect(flatSync.params).toBeUndefined();
        expect(flatSync.shadow_generation).toEqual(expect.any(Number));
        expect(flatSync.expected_shadow_seq).toEqual(expect.any(Number));
        expect(flatSync.watermarks).toBeUndefined();
        expect(flatSync.m0_mutations).toBeUndefined();
        expect(stateSync).toEqual(
            expect.objectContaining({
                watermarks: expect.objectContaining({
                    compartment_sequence: expect.any(Number),
                    memory_id: expect.any(Number),
                    m0_mutation_id: expect.any(Number),
                    memory_mutation_id: expect.any(Number),
                    last_todo_state_hash: expect.any(String),
                }),
            }),
        );
        expect(flatSync.compartments).toEqual([
            {
                sequence: 1,
                start_message: 1,
                end_message: 2,
                start_message_id: "m1#0",
                end_message_id: "m2#0",
                start_date: "2025-01-02",
                end_date: "2025-01-03",
                title: "Populated compartment",
                content: "full content",
                p1: "tier one",
                p2: "tier two",
                p3: "tier three",
                p4: "tier four",
                importance: 73,
                episode_type: "feature",
                legacy: 0,
                created_at: expect.any(Number),
            },
        ]);

        const flatTransform = __shadowSenderTest.toFlatWireBody(transformBody) as Record<
            string,
            unknown
        >;
        expect(flatTransform.method).toBe("shadow_transform");
        expect(flatTransform.params).toBeUndefined();
        expect(flatTransform.shadow_generation).toEqual(expect.any(Number));
        expect(flatTransform.seed_pass).toBe(false);
        expect(flatTransform.pass_inputs).toEqual(
            expect.objectContaining({ now_ms: expect.any(Number) }),
        );
    });

    it("syncs the visible workspace memory union with real project attribution", async () => {
        useTempDataHome("shadow-workspace-");
        const sessionId = "s-workspace";
        const projectPath = "/workspace/owner";
        createOpenCodeDb(sessionId, [{ id: "m1", role: "user", text: "one" }]);
        const db = openDatabase();
        db.prepare(
            `INSERT INTO workspaces (name, created_at, updated_at, share_categories)
             VALUES ('Workspace', 1, 1, '["CONSTRAINTS"]')`,
        ).run();
        const workspaceId = Number(
            (
                db.prepare("SELECT id FROM workspaces WHERE name = 'Workspace'").get() as {
                    id: number;
                }
            ).id,
        );
        const insertMember = db.prepare(
            `INSERT INTO workspace_members
                (workspace_id, project_path, display_name, display_path, added_at)
             VALUES (?, ?, ?, ?, 1)`,
        );
        insertMember.run(workspaceId, projectPath, "owner", projectPath);
        insertMember.run(workspaceId, "/workspace/foreign", "foreign", "/workspace/foreign");
        const own = insertMemory(db, {
            projectPath,
            category: "ARCHITECTURE",
            content: "owner architecture",
        });
        const shared = insertMemory(db, {
            projectPath: "/workspace/foreign",
            category: "CONSTRAINTS",
            content: "shared foreign constraint",
        });
        insertMemory(db, {
            projectPath: "/workspace/foreign",
            category: "NAMING",
            content: "private foreign naming convention",
        });
        const state = __shadowSenderTest.createSessionQueueState();

        const sync = await __shadowSenderTest.buildStateSyncPayload({
            state,
            pass: basePass({ db, sessionId, projectPath }),
            force: true,
        });
        if (
            sync === null ||
            sync === "m0_mutation" ||
            sync === "mismatch" ||
            sync === "unresolved" ||
            sync === "seed_budget"
        ) {
            throw new Error(`unexpected workspace sync result: ${sync}`);
        }
        const body = __shadowSenderTest.toFlatWireBody(sync) as {
            workspace: {
                fingerprint: string;
                members: Array<{ project_path: string; share_categories: string[] }>;
            };
            memories: Array<{ id: number; project_path: string; content: string }>;
        };

        expect(body.workspace.members).toEqual([
            { project_path: projectPath, share_categories: ["CONSTRAINTS"] },
            { project_path: "/workspace/foreign", share_categories: ["CONSTRAINTS"] },
        ]);
        expect(body.workspace.fingerprint).toMatch(/^[a-f0-9]{64}$/);
        expect(body.memories).toEqual([
            expect.objectContaining({ id: own.id, project_path: projectPath }),
            expect.objectContaining({
                id: shared.id,
                project_path: "/workspace/foreign",
                content: "shared foreign constraint",
            }),
        ]);
    });

    it("resends a dropped sync before the next transform and gates after peer rejects", async () => {
        useTempDataHome("shadow-acks-");
        const sessionId = "s-ack";
        createOpenCodeDb(sessionId, [
            { id: "m1", role: "user", text: "one" },
            { id: "m2", role: "user", text: "two" },
            { id: "m3", role: "user", text: "three" },
        ]);
        const db = openDatabase();
        const transport = new FakeTransport();
        transport.syncFailuresRemaining = 1;
        transport.rejectNextTransform = true;
        transport.blockFirstReset();
        const sender = createShadowSender({ transport });

        const msg1 = message(sessionId, "m1", "one");
        const msg2 = message(sessionId, "m2", "two");
        const msg3 = message(sessionId, "m3", "three");
        sender.enqueue(
            basePass({ db, sessionId, inputMessages: [msg1], outputMessages: [msg1], nowMs: 1 }),
        );
        await waitFor(
            () => transport.calls.filter((call) => call.method === "shadow_reset").length === 1,
        );
        sender.enqueue(
            basePass({ db, sessionId, inputMessages: [msg2], outputMessages: [msg2], nowMs: 2 }),
        );
        transport.releaseReset?.();
        await waitFor(() => sender.getStats(sessionId).send_failures >= 2);
        sender.enqueue(
            basePass({ db, sessionId, inputMessages: [msg3], outputMessages: [msg3], nowMs: 3 }),
        );
        await waitFor(
            () => transport.calls.filter((call) => call.method === "shadow_reset").length >= 2,
        );
        await waitFor(
            () => transport.calls.filter((call) => call.method === "shadow_transform").length === 2,
        );

        const methods = transport.calls.map((call) => call.method);
        const firstTransform = methods.indexOf("shadow_transform");
        expect(methods.slice(0, firstTransform)).toEqual([
            "shadow_reset",
            "state_sync",
            "shadow_reset",
            "state_sync",
        ]);
        expect(methods.filter((method) => method === "shadow_reset")).toHaveLength(3);
    });

    it("resets and fully reseeds after a recomp mutation recreates acknowledged sequences", async () => {
        useTempDataHome("shadow-recomp-");
        const sessionId = "s-recomp";
        createOpenCodeDb(sessionId, [
            { id: "m1", role: "user", text: "one" },
            { id: "m2", role: "assistant", text: "two" },
            { id: "m3", role: "user", text: "three" },
        ]);
        const db = openDatabase();
        const compartments = [
            {
                sequence: 0,
                startMessage: 1,
                endMessage: 1,
                startMessageId: "m1",
                endMessageId: "m1",
                title: "one",
                content: "original one",
            },
            {
                sequence: 1,
                startMessage: 2,
                endMessage: 2,
                startMessageId: "m2",
                endMessageId: "m2",
                title: "two",
                content: "original two",
            },
            {
                sequence: 2,
                startMessage: 3,
                endMessage: 3,
                startMessageId: "m3",
                endMessageId: "m3",
                title: "three",
                content: "original three",
            },
        ];
        appendCompartments(db, sessionId, compartments);
        const transport = new FakeTransport();
        const sender = createShadowSender({ transport });
        const msg = message(sessionId, "m3", "three");

        sender.enqueue(
            basePass({ db, sessionId, inputMessages: [msg], outputMessages: [msg], nowMs: 1 }),
        );
        await waitFor(
            () => transport.calls.filter((call) => call.method === "shadow_transform").length === 1,
        );

        replaceAllCompartmentState(
            db,
            sessionId,
            compartments.map((compartment) =>
                compartment.sequence === 2
                    ? { ...compartment, content: "recreated sequence two" }
                    : compartment,
            ),
            [],
        );
        queueM0Mutation(db, {
            sessionId,
            mutationType: "recomp_boundary_change",
            targetId: 2,
            queuedAt: 2,
        });
        sender.enqueue(
            basePass({ db, sessionId, inputMessages: [msg], outputMessages: [msg], nowMs: 2 }),
        );
        await waitFor(
            () => transport.calls.filter((call) => call.method === "shadow_transform").length === 2,
        );

        expect(transport.calls.map((call) => call.method)).toEqual([
            "shadow_reset",
            "state_sync",
            "shadow_transform",
            "shadow_reset",
            "state_sync",
            "shadow_transform",
        ]);
        const syncBodies = transport.calls
            .filter((call) => call.method === "state_sync")
            .map(
                (call) =>
                    call.body as { compartments: Array<{ sequence: number; content: string }> },
            );
        expect(syncBodies[1].compartments).toHaveLength(3);
        expect(syncBodies[1].compartments).toContainEqual(
            expect.objectContaining({ sequence: 2, content: "recreated sequence two" }),
        );
    });

    it("retries a reset on the next pass after a queued reset fails", async () => {
        useTempDataHome("shadow-reset-retry-");
        const sessionId = "s-reset-retry";
        createOpenCodeDb(sessionId, [{ id: "m1", role: "user", text: "one" }]);
        const db = openDatabase();
        const transport = new FakeTransport();
        transport.resetFailuresRemaining = 1;
        const sender = createShadowSender({ transport });

        sender.resetSession(sessionId, "route_reopen");
        await waitFor(() => sender.getStats(sessionId).send_failures === 1);
        const msg = message(sessionId, "m1", "one");
        sender.enqueue(
            basePass({ db, sessionId, inputMessages: [msg], outputMessages: [msg], nowMs: 1 }),
        );
        await waitFor(
            () => transport.calls.filter((call) => call.method === "shadow_transform").length === 1,
        );

        expect(transport.calls.map((call) => call.method)).toEqual([
            "shadow_reset",
            "shadow_reset",
            "state_sync",
            "shadow_transform",
        ]);
    });

    it("drops concurrent transport work instead of retaining bodies behind a stalled call", async () => {
        const transport = new __shadowSenderTest.SubcShadowTransport(
            "/unused/connection.json",
            "magic-context",
            1_000,
        );
        const internals = transport as unknown as {
            ensureRoute: () => Promise<number>;
        };
        let releaseRoute: ((route: number) => void) | undefined;
        internals.ensureRoute = () =>
            new Promise((resolve) => {
                releaseRoute = resolve;
            });
        const controller = new AbortController();
        const first = transport.call({
            sessionId: "s-busy-one",
            projectRoot: "/tmp/project",
            method: "state_sync",
            body: { large: "first" },
            signal: controller.signal,
        });

        await expect(
            transport.call({
                sessionId: "s-busy-two",
                projectRoot: "/tmp/project",
                method: "state_sync",
                body: { large: "must-not-queue" },
            }),
        ).rejects.toThrow("transport busy");

        controller.abort(new Error("test complete"));
        releaseRoute?.(1);
        await expect(first).rejects.toThrow("test complete");
    });

    it("keeps sender exceptions off the transform hot path", () => {
        const throwingSender = createShadowSender({
            transport: {
                async call() {
                    throw new Error("boom");
                },
            },
        });
        expect(() => {
            throwingSender.enqueue(
                basePass({
                    db: {
                        prepare: () => {
                            throw new Error("db not used before enqueue returns");
                        },
                    } as unknown as ContextDatabase,
                    sessionId: "s-throw",
                }),
            );
        }).not.toThrow();
    });
});
