/// <reference types="bun-types" />

import { afterEach, describe, expect, it } from "bun:test";
import { EventEmitter } from "node:events";
import { mkdirSync, mkdtempSync, rmSync } from "node:fs";
import type { Socket } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
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
import { appendAutoSearchHintDecision } from "../../features/magic-context/storage-meta-persisted";
import { Database } from "../../shared/sqlite";
import { closeQuietly } from "../../shared/sqlite-helpers";
import { primeInMemoryTailRawMessageCache, withRawSessionMessageCache } from "./read-session-chunk";
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
            const timestamp = index + 1;
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
}): ShadowTransformPass {
    const inputMessages = args.inputMessages ?? [message(args.sessionId, "m1", "hello")];
    return {
        sessionId: args.sessionId,
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

class FakeTransport implements ShadowTransport {
    calls: Array<{ method: string; body: unknown }> = [];
    syncFailuresRemaining = 0;
    rejectNextTransform = false;
    resetFailuresRemaining = 0;
    seq = 0;
    releaseReset: (() => void) | null = null;
    private resetGate: Promise<void> | null = null;

    blockFirstReset(): void {
        this.resetGate = new Promise((resolve) => {
            this.releaseReset = resolve;
        });
    }

    async call(req: { method: string; body: unknown }): Promise<unknown> {
        this.calls.push({ method: req.method, body: req.body });
        if (req.method === "shadow_reset" && this.resetGate) {
            await this.resetGate;
            this.resetGate = null;
        }
        if (req.method === "shadow_reset" && this.resetFailuresRemaining > 0) {
            this.resetFailuresRemaining -= 1;
            throw new Error("reset interrupted");
        }
        if (req.method === "state_sync" && this.syncFailuresRemaining > 0) {
            this.syncFailuresRemaining -= 1;
            throw new Error("dropped sync");
        }
        if (req.method === "shadow_transform" && this.rejectNextTransform) {
            this.rejectNextTransform = false;
            const error = new Error("generation mismatch") as Error & { code?: string };
            error.code = "stale_generation";
            throw error;
        }
        if (req.method === "shadow_reset") {
            this.seq = 0;
            return { result: { shadow_generation: 1, shadow_seq: 0 } };
        }
        if (req.method === "state_sync") {
            this.seq += 1;
            const watermarks = (req.body as { params?: { watermarks?: unknown } }).params
                ?.watermarks;
            return { result: { shadow_seq: this.seq, watermarks } };
        }
        if (req.method === "shadow_transform") {
            this.seq += 1;
            return { result: { shadow_seq: this.seq } };
        }
        return { result: {} };
    }
}

class FakeSocket extends EventEmitter {
    destroyed = false;
    onWrite: ((chunk: Buffer) => void) | null = null;

    write(chunk: Uint8Array): boolean {
        this.onWrite?.(Buffer.from(chunk));
        return true;
    }

    destroy(): this {
        if (this.destroyed) return this;
        this.destroyed = true;
        this.emit("close");
        return this;
    }
}

function responseFrame(channel: number, corr: number, body: unknown): Buffer {
    const payload = Buffer.from(JSON.stringify(body));
    const header = Buffer.alloc(17);
    header.writeUInt32LE(payload.length, 0);
    header.writeUInt8(1, 4);
    header.writeUInt8(1, 5);
    header.writeUInt16LE(channel, 7);
    header.writeBigUInt64LE(BigInt(corr), 9);
    return Buffer.concat([header, payload]);
}

async function waitFor(predicate: () => boolean): Promise<void> {
    const deadline = Date.now() + 1_000;
    while (Date.now() < deadline) {
        if (predicate()) return;
        await new Promise((resolve) => setTimeout(resolve, 5));
    }
    expect(predicate()).toBe(true);
}

describe("shadow sender", () => {
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

    it("resolves canonical raw ordinals, detects mismatches, and skips unresolvable windows", () => {
        useTempDataHome("shadow-ordinals-");
        const sessionId = "s-ord";
        createOpenCodeDb(sessionId, [
            { id: "m1", role: "user", text: "old" },
            { id: "summary", role: "assistant", text: "summary", summary: true },
            { id: "m2", role: "assistant", text: "visible" },
        ]);
        const state = __shadowSenderTest.createSessionQueueState();

        const resolved = __shadowSenderTest.resolveOrdinalsForShadow({
            sessionId,
            messages: [message(sessionId, "m2", "visible")],
            generation: state.shadowGeneration,
            memoGeneration: state.idOrdinalMemoGeneration,
            memo: state.idOrdinalMemo,
        });
        expect(resolved).toEqual(
            expect.objectContaining({
                ok: true,
                annotatedInput: [expect.objectContaining({ absolute_ordinal: 2 })],
            }),
        );

        const belowFloor = withRawSessionMessageCache(() => {
            primeInMemoryTailRawMessageCache({
                sessionId,
                messages: [{ ordinal: 2, id: "m2", role: "assistant", parts: [] }],
                absoluteMessageCount: 2,
            });
            return __shadowSenderTest.resolveOrdinalsForShadow({
                sessionId,
                messages: [message(sessionId, "m1", "old")],
                generation: state.shadowGeneration,
                memoGeneration: state.idOrdinalMemoGeneration,
                memo: new Map(),
            });
        });
        expect(belowFloor).toEqual(
            expect.objectContaining({
                ok: true,
                annotatedInput: [expect.objectContaining({ absolute_ordinal: 1 })],
            }),
        );

        createOpenCodeDb(sessionId, [{ id: "m3", role: "assistant", text: "new" }]);
        expect(
            __shadowSenderTest.resolveOrdinalsForShadow({
                sessionId,
                messages: [message(sessionId, "m2", "visible")],
                generation: state.shadowGeneration,
                memoGeneration: state.idOrdinalMemoGeneration,
                memo: state.idOrdinalMemo,
            }),
        ).toEqual(expect.objectContaining({ ok: false, reason: "unresolved" }));

        state.idOrdinalMemo.set("m2", 7);
        createOpenCodeDb(sessionId, [
            { id: "m1", role: "user", text: "old" },
            { id: "m2", role: "assistant", text: "visible" },
        ]);
        expect(
            __shadowSenderTest.resolveOrdinalsForShadow({
                sessionId,
                messages: [message(sessionId, "m2", "visible")],
                generation: state.shadowGeneration,
                memoGeneration: state.idOrdinalMemoGeneration,
                memo: state.idOrdinalMemo,
            }),
        ).toEqual(expect.objectContaining({ ok: false, reason: "mismatch" }));
    });

    it("runs one in-flight operation per session and drops the oldest queued pass above the cap", async () => {
        useTempDataHome("shadow-fifo-");
        const sessionId = "s-fifo";
        createOpenCodeDb(sessionId, [
            { id: "m1", role: "user", text: "one" },
            { id: "m2", role: "user", text: "two" },
            { id: "m3", role: "user", text: "three" },
            { id: "m4", role: "user", text: "four" },
            { id: "m5", role: "user", text: "five" },
            { id: "m6", role: "user", text: "six" },
        ]);
        const db = openDatabase();
        const transport = new FakeTransport();
        transport.blockFirstReset();
        const sender = createShadowSender({ transport });

        for (let index = 1; index <= 6; index += 1) {
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
        expect(sender.getStats(sessionId).dropped_oldest).toBe(1);
        transport.releaseReset?.();
        await waitFor(
            () => transport.calls.filter((call) => call.method === "shadow_transform").length === 5,
        );

        // Wire bodies are FLAT (op fields beside `method`) — the module's serde
        // parsers reject a nested `params` object with invalid_params.
        const sentNowMs = transport.calls
            .filter((call) => call.method === "shadow_transform")
            .map((call) => (call.body as { pass_inputs: { now_ms: number } }).pass_inputs.now_ms);
        expect(sentNowMs).toEqual([1, 3, 4, 5, 6]);
    });

    it("serializes state_sync and shadow_transform with the shadow wire field inventory", () => {
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
            compartment_sequence: -1,
            memory_id: 0,
            m0_mutation_id: 0,
            memory_mutation_id: 0,
            last_todo_state_hash: "",
        };
        const pass = basePass({ db, sessionId });

        const stateSync = __shadowSenderTest.buildStateSyncPayload({
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
        expect(flatTransform.pass_inputs).toEqual(
            expect.objectContaining({ now_ms: expect.any(Number) }),
        );
    });

    it("syncs the visible workspace memory union with real project attribution", () => {
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

        const sync = __shadowSenderTest.buildStateSyncPayload({
            state,
            pass: basePass({ db, sessionId, projectPath }),
            force: true,
        });
        if (
            sync === null ||
            sync === "m0_mutation" ||
            sync === "mismatch" ||
            sync === "unresolved"
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
        const sender = createShadowSender({ transport });

        const msg1 = message(sessionId, "m1", "one");
        const msg2 = message(sessionId, "m2", "two");
        const msg3 = message(sessionId, "m3", "three");
        sender.enqueue(
            basePass({ db, sessionId, inputMessages: [msg1], outputMessages: [msg1], nowMs: 1 }),
        );
        sender.enqueue(
            basePass({ db, sessionId, inputMessages: [msg2], outputMessages: [msg2], nowMs: 2 }),
        );
        sender.enqueue(
            basePass({ db, sessionId, inputMessages: [msg3], outputMessages: [msg3], nowMs: 3 }),
        );

        await waitFor(() => sender.getStats(sessionId).send_failures >= 2);
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
            "state_sync",
        ]);
        expect(methods.filter((method) => method === "shadow_reset")).toHaveLength(2);
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

        sender.resetSession(sessionId, "manual_recovery");
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

    it("destroys a timed-out socket and succeeds immediately on a clean socket", async () => {
        const first = new FakeSocket();
        const second = new FakeSocket();
        second.onWrite = () => {
            queueMicrotask(() =>
                second.emit("data", responseFrame(1, 2, { result: { ok: true } })),
            );
        };
        const sockets = [first, second];
        const transport = new __shadowSenderTest.SubcShadowTransport(
            "/unused/connection.json",
            "magic-context",
            20,
        );
        const internals = transport as unknown as {
            socket: Socket | null;
            reader: unknown;
            ensureConnected: () => Promise<void>;
            ensureRoute: () => Promise<number>;
        };
        let nextSocket = 0;
        internals.ensureRoute = async () => 1;
        internals.ensureConnected = async () => {
            if (internals.socket && !internals.socket.destroyed && internals.reader) return;
            const socket = sockets[nextSocket++];
            internals.socket = socket as unknown as Socket;
            internals.reader = new __shadowSenderTest.SocketReader(socket as unknown as Socket);
        };
        const request = {
            sessionId: "s-socket",
            projectRoot: "/tmp/project",
            method: "shadow_transform" as const,
            body: { method: "shadow_transform" },
        };

        await expect(transport.call(request)).rejects.toThrow("read timeout");
        expect(first.destroyed).toBe(true);
        await expect(transport.call(request)).resolves.toEqual({ result: { ok: true } });
        expect(nextSocket).toBe(2);
        expect(second.destroyed).toBe(false);
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
