/// <reference types="bun-types" />

import { afterEach, describe, expect, it, mock } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import {
    type AuthorityStatus,
    getAuthorityManagedMarker,
} from "../../features/magic-context/context-authority";
import { runMigrations } from "../../features/magic-context/migrations";
import type { ContextDatabase } from "../../features/magic-context/storage";
import { getChannel2NudgeState, setChannel2NudgeState } from "../../features/magic-context/storage";
import { initializeDatabase } from "../../features/magic-context/storage-db";
import { getOrCreateSessionMeta } from "../../features/magic-context/storage-meta";
import { createMessagesTransformHandler } from "../../plugin/messages-transform";
import { Database, withPrivilegedWriter } from "../../shared/sqlite";
import { closeQuietly } from "../../shared/sqlite-helpers";
import { setRawMessageProvider } from "./read-session-chunk";
import { closeReadOnlySessionDb } from "./read-session-db";
import {
    __rustModeTransformTest,
    createRustModeTransform as createRustModeTransformImpl,
    type RustModeModuleClient,
} from "./rust-mode-transform";
import type { TransformDeps } from "./transform";
import { createTransform } from "./transform";
import type { MessageLike } from "./transform-operations";

const createRustModeTransform = (
    deps: TransformDeps,
    options: Parameters<typeof createRustModeTransformImpl>[1],
) =>
    createRustModeTransformImpl(deps, {
        ...options,
        allowAuthorityProtocolBypassForTests: true,
    });

const sessions: string[] = [];
const databases: ContextDatabase[] = [];
const unregisters: Array<() => void> = [];
const availabilityDataHomes: string[] = [];
const originalXdgDataHome = process.env.XDG_DATA_HOME;

afterEach(() => {
    closeReadOnlySessionDb();
    for (const unregister of unregisters.splice(0)) unregister();
    for (const dataHome of availabilityDataHomes.splice(0)) {
        rmSync(dataHome, { recursive: true, force: true });
    }
    if (originalXdgDataHome === undefined) delete process.env.XDG_DATA_HOME;
    else process.env.XDG_DATA_HOME = originalXdgDataHome;
    for (const db of databases.splice(0)) closeQuietly(db);
});

function makeDb(): ContextDatabase {
    const db = new Database(":memory:") as ContextDatabase;
    initializeDatabase(db);
    runMigrations(db);
    databases.push(db);
    return db;
}

function installRawProvider(sessionId: string): void {
    const row = {
        id: "m1",
        timeCreated: 1,
        contributesOrdinal: true,
        hasValidInfo: true,
    };
    unregisters.push(
        setRawMessageProvider(sessionId, {
            readMessages: () => [row],
            readMessageOrdinalPage: (after, limit) =>
                !after || row.timeCreated > after.timeCreated || row.id > after.id
                    ? [row].slice(0, limit)
                    : [],
            getStoredMessageCount: () => 1,
            readMessagePartsById: () => ({
                id: "m1",
                role: "user",
                parts: [{ type: "text", text: "hello" }],
                createdAt: 1,
            }),
        }),
    );
}

function makeMessages(sessionId: string): MessageLike[] {
    return [
        {
            info: { id: "m1", role: "user", sessionID: sessionId },
            parts: [{ type: "text", text: "hello" }],
        },
    ];
}

function makeDeps(db: ContextDatabase, moduleClient: RustModeModuleClient): TransformDeps {
    return {
        tagger: {} as TransformDeps["tagger"],
        scheduler: {} as TransformDeps["scheduler"],
        contextUsageMap: new Map(),
        db,
        protectedTags: 4,
        clearReasoningAge: 50,
        historyRefreshSessions: new Set(),
        pendingMaterializationSessions: new Set(),
        lastHeuristicsTurnId: new Map(),
        directory: "/tmp/project",
        projectPath: "/tmp/project",
        memoryConfig: { enabled: false, injectionBudgetTokens: 1000, autoPromote: false },
        liveModelBySession: new Map(),
        sessionDirectoryBySession: new Map(),
        transformMode: "rust",
        rustModeModuleClient: moduleClient,
        rustModeAllowAuthorityProtocolBypassForTests: true,
    };
}

function makeMeta(
    db: ContextDatabase,
    sessionId: string,
): ReturnType<typeof getOrCreateSessionMeta> {
    return getOrCreateSessionMeta(db, sessionId);
}

function installAvailabilityDb(sessionId: string, firstUserTools?: Record<string, unknown>): void {
    const dataHome = mkdtempSync(join(tmpdir(), "rust-mode-availability-"));
    availabilityDataHomes.push(dataHome);
    const dbPath = join(dataHome, "opencode", "opencode.db");
    mkdirSync(dirname(dbPath), { recursive: true });
    const opencodeDb = new Database(dbPath);
    opencodeDb.exec(`
        CREATE TABLE message (
            id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            time_created INTEGER NOT NULL,
            time_updated INTEGER NOT NULL,
            data TEXT NOT NULL
        );
    `);
    if (firstUserTools !== undefined) {
        opencodeDb
            .prepare(
                "INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES (?, ?, ?, ?, ?)",
            )
            .run(
                "availability-user",
                sessionId,
                1,
                1,
                JSON.stringify({ id: "availability-user", role: "user", tools: firstUserTools }),
            );
    }
    closeQuietly(opencodeDb);
    process.env.XDG_DATA_HOME = dataHome;
}

function authoritySeqMismatch(durableSeq: number): Error & {
    code: string;
} {
    const error = new Error(
        JSON.stringify({
            code: "authority_seq_mismatch",
            durable_authority_seq: durableSeq,
        }),
    ) as Error & { code: string };
    error.code = "authority_seq_mismatch";
    return error;
}

describe("Rust mode authority adapter", () => {
    it("uses the resolved session directory instead of the plugin launch directory for authority routes", async () => {
        const sessionId = "ses-directory-root";
        installRawProvider(sessionId);
        const db = makeDb();
        withPrivilegedWriter(db, () => {
            db.prepare(
                "INSERT INTO memories (project_path, category, content, normalized_hash, first_seen_at, created_at, updated_at, last_seen_at) VALUES (?, 'CONSTRAINTS', 'seed me', 'seed-hash', 0, 0, 0, 0)",
            ).run("git:identity");
        });
        const authorityRoots: string[] = [];
        const statuses = new Map<string, AuthorityStatus>();
        const module: RustModeModuleClient = {
            call: async () => {
                throw new Error("stop after authority preparation");
            },
            authorityStatus: async (args) => {
                authorityRoots.push(String(args.projectRoot));
                return { authority: statuses.get(args.domain) ?? null };
            },
            authorityPrepare: async (args) => {
                authorityRoots.push(String(args.projectRoot));
                const domain = String(args.domain) as "memories" | "notes";
                const phase = String(args.phase);
                const base = {
                    context_store_uuid: String(args.context_store_uuid),
                    project: String(args.project),
                    domain,
                    generation: 1,
                };
                if (phase === "begin") {
                    const authority = { ...base, state: "PREPARING" as const };
                    statuses.set(domain, authority);
                    return { authority };
                }
                if (phase === "complete") {
                    const checksum = String(args.checksum_expected);
                    const authority = {
                        ...base,
                        state: "PREPARING" as const,
                        checksum_expected: checksum,
                        checksum_actual: checksum,
                        checksum_ok: true,
                    };
                    statuses.set(domain, authority);
                    return { authority };
                }
                if (phase === "ack") {
                    const authority = { ...base, state: "MODULE" as const };
                    statuses.set(domain, authority);
                    return { authority };
                }
                const authority = { ...base, state: "TS" as const };
                statuses.set(domain, authority);
                return { authority };
            },
            authoritySeed: async (args) => {
                authorityRoots.push(String(args.projectRoot));
                const rows = Array.isArray(args.rows) ? args.rows : [];
                return { seeded: rows.length, module_row_ids: rows.map((_, index) => index + 1) };
            },
            mirrorPull: async (args) => {
                authorityRoots.push(String(args.projectRoot));
                return {
                    page: {
                        domain: args.domain,
                        cursor: args.cursor,
                        next_cursor: args.cursor,
                        has_more: false,
                        rows: [],
                    },
                };
            },
        };
        const deps = makeDeps(db, module);
        deps.directory = "/launch/root-a";
        deps.projectPath = "git:identity";
        deps.sessionDirectoryBySession?.set(sessionId, "/session/root-b");
        const runner = createRustModeTransformImpl(deps, { moduleClient: module });
        const messages = makeMessages(sessionId);

        await runner.run(sessionId, messages, { messages: [...messages] }, makeMeta(db, sessionId));

        expect(authorityRoots.length).toBeGreaterThan(0);
        expect(authorityRoots.every((root) => root === "/session/root-b")).toBe(true);
    });

    it("copies the resolved history budget onto the authority wire", () => {
        const body = __rustModeTransformTest.buildTransformBody({
            sessionId: "budget-wire",
            input: [],
            nativeMessages: [],
            passInputs: { history_budget_tokens: 42_000 },
            usage: {},
            modelKey: null,
            providerId: null,
            midTurn: false,
        });
        expect(body.history_budget_tokens).toBe(42_000);
    });

    it("adopts a durable sequence from a fresh process and retries the sync", async () => {
        const sessionId = `rust-adopt-${Date.now()}`;
        sessions.push(sessionId);
        const db = makeDb();
        installRawProvider(sessionId);
        const native = [{ role: "assistant", parts: [{ type: "text", text: "module output" }] }];
        const methods: string[] = [];
        let firstSync = true;
        const moduleClient: RustModeModuleClient = {
            call: async ({ method }) => {
                methods.push(method);
                if (method === "state_sync" && firstSync) {
                    firstSync = false;
                    throw authoritySeqMismatch(5);
                }
                return method === "transform" ? { native_messages: native } : { ok: true };
            },
        };
        const transform = createRustModeTransform(makeDeps(db, moduleClient), { moduleClient });
        const messages = makeMessages(sessionId);
        const output = { messages: messages as unknown[] };

        await transform.run(sessionId, messages, output, makeMeta(db, sessionId));

        expect(methods).toEqual(["state_sync", "state_sync", "transform"]);
        expect(transform.getState(sessionId).lastAckedSeq).toBe(6);
        expect(transform.getState(sessionId).lastAckedWatermarks).not.toBeNull();
        expect(output.messages).toEqual(native);
    });

    it("fails after the second authority mismatch in one transform pass", async () => {
        const sessionId = `rust-adopt-once-${Date.now()}`;
        sessions.push(sessionId);
        const db = makeDb();
        installRawProvider(sessionId);
        const messages = makeMessages(sessionId);
        const methods: string[] = [];
        const moduleClient: RustModeModuleClient = {
            call: async ({ method }) => {
                methods.push(method);
                if (method === "state_sync") throw authoritySeqMismatch(4);
                return { native_messages: [] };
            },
        };
        const transform = createRustModeTransform(makeDeps(db, moduleClient), { moduleClient });
        const output = { messages: messages as unknown[] };

        await transform.run(sessionId, messages, output, makeMeta(db, sessionId));

        expect(methods).toEqual(["state_sync", "state_sync"]);
        expect(transform.getState(sessionId).lastAckedSeq).toBe(4);
        expect(transform.getState(sessionId).lastAckedWatermarks).toBeNull();
        expect(output.messages).toBe(messages);
    });

    it("gates the transform before any TypeScript mutation", async () => {
        const sessionId = `rust-gate-${Date.now()}`;
        sessions.push(sessionId);
        const db = makeDb();
        installRawProvider(sessionId);
        const input = makeMessages(sessionId);
        const native = [{ role: "user", parts: [{ type: "text", text: "unchanged" }] }];
        const moduleClient: RustModeModuleClient = {
            call: async ({ method }) =>
                method === "transform" ? { native_messages: native } : { ok: true },
        };
        const deps = makeDeps(db, moduleClient);
        const transform = createTransform(deps);
        const output = { messages: input as unknown[] };
        await transform({}, output);
        expect(output.messages).toEqual(native);
        expect(input).toEqual(native);
    });

    it("applies module output through the OpenCode hook array reference", async () => {
        const sessionId = `rust-hook-array-${Date.now()}`;
        sessions.push(sessionId);
        const db = makeDb();
        installRawProvider(sessionId);
        const input = makeMessages(sessionId);
        const native = [
            {
                info: { role: "user", sessionID: sessionId },
                parts: [{ type: "text", text: "<project-docs>m0</project-docs>", synthetic: true }],
            },
            {
                info: { id: "m1", role: "user", sessionID: sessionId },
                parts: [{ type: "text", text: "tail" }],
            },
        ];
        const moduleClient: RustModeModuleClient = {
            call: async ({ method }) =>
                method === "transform"
                    ? {
                          action: "CACHE_HIT",
                          served_from: "transform",
                          boundary_id: "m1#0",
                          native_messages: native,
                      }
                    : { ok: true },
        };
        const transform = createTransform(makeDeps(db, moduleClient));
        const handler = createMessagesTransformHandler({
            magicContext: {
                "experimental.chat.messages.transform": transform as never,
            },
        });
        const output = { messages: input as unknown[] };
        const callerHeldMessages = output.messages;

        const returned = await handler({}, output as never);

        expect(returned).toEqual(native);
        expect(output.messages).toEqual(native);
        expect(callerHeldMessages).toEqual(native);
        expect(output.messages).toBe(callerHeldMessages);
    });

    it("fails the pass when a present boundary lacks a synthetic session-scoped m0", async () => {
        const sessionId = `rust-wire-invariant-${Date.now()}`;
        sessions.push(sessionId);
        const db = makeDb();
        installRawProvider(sessionId);
        const input = makeMessages(sessionId);
        const moduleClient: RustModeModuleClient = {
            call: async ({ method }) =>
                method === "transform"
                    ? {
                          action: "CACHE_HIT",
                          served_from: "transform",
                          boundary_id: "m1#0",
                          native_messages: [
                              {
                                  info: { role: "user", sessionID: sessionId },
                                  parts: [{ type: "text", text: "not marked synthetic" }],
                              },
                          ],
                      }
                    : { ok: true },
        };
        const transform = createRustModeTransform(makeDeps(db, moduleClient), { moduleClient });
        const output = { messages: input as unknown[] };

        await transform.run(sessionId, input, output, makeMeta(db, sessionId));

        expect(output.messages).toBe(input);
        expect(output.messages[0]).toEqual(input[0]);
        expect(transform.getState(sessionId).failureCount).toBe(1);
    });

    it("seeds before the first transform and applies native output verbatim", async () => {
        const sessionId = `rust-seed-${Date.now()}`;
        sessions.push(sessionId);
        const db = makeDb();
        installAvailabilityDb(sessionId, {});
        installRawProvider(sessionId);
        const native = [{ role: "user", parts: [{ type: "text", text: "module output" }] }];
        const methods: string[] = [];
        let transformRequest: Record<string, unknown> | undefined;
        const moduleClient: RustModeModuleClient = {
            call: async ({ method, body }) => {
                methods.push(method);
                if (method === "transform") transformRequest = body as Record<string, unknown>;
                return method === "transform" ? { native_messages: native } : { ok: true };
            },
        };
        const transform = createRustModeTransform(makeDeps(db, moduleClient), { moduleClient });
        const messages = makeMessages(sessionId);
        const output = { messages: messages as unknown[] };

        await transform.run(sessionId, messages, output, makeMeta(db, sessionId));
        expect(methods).toEqual(["state_sync", "transform"]);
        expect(transformRequest?.serve_native).toBe(true);
        expect(transformRequest?.tool_present).toBe(true);
        expect(transformRequest?.native_messages).toBe(messages);
        expect(Array.isArray(transformRequest?.messages)).toBe(true);
        expect(output.messages).toEqual(native);

        methods.length = 0;
        const secondInput = makeMessages(sessionId);
        const secondOutput = { messages: secondInput as unknown[] };
        await transform.run(sessionId, secondInput, secondOutput, makeMeta(db, sessionId));
        expect(methods).toEqual(["transform"]);
        expect(secondOutput.messages).toEqual(native);
    });

    it("preserves the receiver for a class-backed compartment mirror client", async () => {
        const sessionId = `rust-class-compartments-${Date.now()}`;
        sessions.push(sessionId);
        const db = makeDb();
        installRawProvider(sessionId);

        class ClassBackedModuleClient {
            private readonly title = "receiver-bound compartment";

            async call({ method }: Parameters<RustModeModuleClient["call"]>[0]) {
                return method === "transform" ? { native_messages: [] } : { ok: true };
            }

            async getCompartmentsAfter(_sessionId: string, _afterSequence: number) {
                if (this.title !== "receiver-bound compartment") {
                    throw new Error("class receiver was detached");
                }
                return {
                    max_sequence: 1,
                    compartments: [
                        {
                            sequence: 1,
                            start_message: 0,
                            end_message: 0,
                            start_message_id: "m1",
                            end_message_id: "m1",
                            title: this.title,
                            content: "summary",
                        },
                    ],
                };
            }
        }

        const moduleClient: RustModeModuleClient = new ClassBackedModuleClient();
        const transform = createRustModeTransform(makeDeps(db, moduleClient), { moduleClient });
        const messages = makeMessages(sessionId);
        await transform.run(
            sessionId,
            messages,
            { messages: [...messages] },
            makeMeta(db, sessionId),
        );

        expect(
            db.prepare("SELECT title FROM compartments WHERE session_id = ?").get(sessionId),
        ).toEqual({ title: "receiver-bound compartment" });
    });

    it("sends tool_present false while availability remains provisional", async () => {
        const sessionId = `rust-availability-provisional-${Date.now()}`;
        sessions.push(sessionId);
        installAvailabilityDb(sessionId);
        const db = makeDb();
        installRawProvider(sessionId);
        const requestBodies: Array<Record<string, unknown>> = [];
        const moduleClient: RustModeModuleClient = {
            call: async ({ method, body }) => {
                if (method === "transform") requestBodies.push(body as Record<string, unknown>);
                return method === "transform" ? { native_messages: [] } : { ok: true };
            },
        };
        const transform = createRustModeTransform(makeDeps(db, moduleClient), { moduleClient });
        const messages: MessageLike[] = [
            {
                info: { id: "m1", role: "assistant", sessionID: sessionId },
                parts: [{ type: "text", text: "assistant" }],
            },
        ];

        await transform.run(
            sessionId,
            messages,
            { messages: messages as unknown[] },
            makeMeta(db, sessionId),
        );

        expect(requestBodies).toHaveLength(1);
        expect(requestBodies[0]?.tool_present).toBe(false);
    });

    it("delivers a repeated module directive only once across the synthetic nudge turn", async () => {
        const sessionId = `rust-channel2-refire-${Date.now()}`;
        sessions.push(sessionId);
        const db = makeDb();
        installRawProvider(sessionId);
        const native = [{ role: "assistant", parts: [] }];
        const promptAsync = mock(async () => ({}));
        const moduleClient: RustModeModuleClient = {
            call: async ({ method }) =>
                method === "transform"
                    ? {
                          native_messages: native,
                          host_directives: {
                              channel2_nudge: { text: "drop spent tool output" },
                          },
                      }
                    : { ok: true },
        };
        const transform = createRustModeTransform(makeDeps(db, moduleClient), {
            moduleClient,
            hostClient: {
                session: {
                    messages: async () => ({ data: [] }),
                    promptAsync,
                },
            },
        });

        const firstInput = makeMessages(sessionId);
        await transform.run(
            sessionId,
            firstInput,
            { messages: firstInput },
            makeMeta(db, sessionId),
        );

        const syntheticInput = [
            ...makeMessages(sessionId),
            {
                info: { id: "channel2-nudge-1", role: "user", sessionID: sessionId },
                parts: [{ type: "text", text: "drop spent tool output", synthetic: true }],
            },
        ];
        await transform.run(
            sessionId,
            syntheticInput,
            { messages: syntheticInput },
            makeMeta(db, sessionId),
        );

        expect(getChannel2NudgeState(db, sessionId)).toBe("delivered");
        expect(transform.getState(sessionId).syntheticTurnCount).toBe(1);
    });

    it("breaks a synthetic-turn cascade after three turns", async () => {
        const sessionId = `rust-loop-breaker-${Date.now()}`;
        sessions.push(sessionId);
        const db = makeDb();
        installRawProvider(sessionId);
        const promptAsync = mock(async () => ({}));
        const moduleClient: RustModeModuleClient = {
            call: async ({ method }) =>
                method === "transform"
                    ? {
                          native_messages: [{ role: "assistant", parts: [] }],
                          host_directives: {
                              channel2_nudge: { text: "drop spent tool output" },
                          },
                      }
                    : { ok: true },
        };
        const transform = createRustModeTransform(makeDeps(db, moduleClient), {
            moduleClient,
            hostClient: {
                session: {
                    messages: async () => ({ data: [] }),
                    promptAsync,
                },
            },
        });

        for (let turn = 1; turn <= 4; turn += 1) {
            setChannel2NudgeState(db, sessionId, "pending");
            const input = [
                ...makeMessages(sessionId),
                {
                    info: { id: `synthetic-${turn}`, role: "user", sessionID: sessionId },
                    parts: [{ type: "text", text: "synthetic turn", synthetic: true }],
                },
            ];
            await transform.run(sessionId, input, { messages: input }, makeMeta(db, sessionId));
        }

        expect(promptAsync).toHaveBeenCalledTimes(0);
        expect(transform.getState(sessionId).syntheticTurnCount).toBe(4);
        expect(getChannel2NudgeState(db, sessionId)).toBe("");

        setChannel2NudgeState(db, sessionId, "pending");
        const realInput = makeMessages(sessionId);
        await transform.run(sessionId, realInput, { messages: realInput }, makeMeta(db, sessionId));
        expect(promptAsync).toHaveBeenCalledTimes(1);
        expect(transform.getState(sessionId).syntheticTurnCount).toBe(0);
    });

    it("re-pages every transform payload after need_full_sync", async () => {
        const sessionId = `rust-repage-${Date.now()}`;
        sessions.push(sessionId);
        const db = makeDb();
        installAvailabilityDb(sessionId, {});
        installRawProvider(sessionId);
        const messages = makeMessages(sessionId);
        messages[0]!.parts = [{ type: "text", text: "x".repeat(600_000) }];
        const native = [{ role: "assistant", parts: [] }];
        const transformBodies: Array<Record<string, unknown>> = [];
        let retryStarted = false;
        const moduleClient: RustModeModuleClient = {
            call: async ({ method, body }) => {
                if (method !== "transform") return { ok: true };
                const page = body as Record<string, unknown>;
                transformBodies.push(page);
                if (!retryStarted && page.transform_page_complete === true) {
                    retryStarted = true;
                    return { status: "need_full_sync" };
                }
                return { native_messages: native };
            },
        };
        const transform = createRustModeTransform(makeDeps(db, moduleClient), { moduleClient });
        const output = { messages: messages as unknown[] };
        await transform.run(sessionId, messages, output, makeMeta(db, sessionId));

        const pageIds = new Set(transformBodies.map((body) => body.transform_page_id));
        expect(pageIds.size).toBe(2);
        expect(transformBodies.length).toBeGreaterThan(2);
        expect(
            transformBodies.every((body) =>
                [
                    "transform_page_id",
                    "transform_generation",
                    "transform_page_index",
                    "transform_page_total",
                    "transform_page_complete",
                    "transform_page_digest",
                ].every((field) => field in body),
            ),
        ).toBe(true);
        expect(transformBodies.at(-1)?.tool_present).toBe(true);
        expect(output.messages).toEqual(native);
    });

    it("passes through raw input, parks after three failures, then probes on the fifth pass", async () => {
        const sessionId = `rust-failure-${Date.now()}`;
        sessions.push(sessionId);
        const db = makeDb();
        installRawProvider(sessionId);
        let shouldFail = true;
        let transformCalls = 0;
        let toastCalls = 0;
        const moduleClient: RustModeModuleClient = {
            call: async ({ method }) => {
                if (method === "transform") transformCalls += 1;
                if (shouldFail) throw new Error("daemon unavailable");
                return method === "transform"
                    ? { native_messages: [{ role: "assistant", parts: [] }] }
                    : { ok: true };
            },
        };
        const transform = createRustModeTransform(makeDeps(db, moduleClient), {
            moduleClient,
            notifyParked: () => {
                toastCalls += 1;
            },
        });
        for (let pass = 1; pass <= 3; pass += 1) {
            const input = makeMessages(sessionId);
            const output = { messages: input as unknown[] };
            await transform.run(sessionId, input, output, makeMeta(db, sessionId));
            expect(output.messages).toBe(input);
        }
        expect(transform.getState(sessionId).parked).toBe(true);
        expect(toastCalls).toBe(1);
        shouldFail = false;
        for (let pass = 0; pass < 2; pass += 1) {
            const input = makeMessages(sessionId);
            const output = { messages: input as unknown[] };
            await transform.run(sessionId, input, output, makeMeta(db, sessionId));
            if (pass === 0) expect(output.messages).toBe(input);
        }
        expect(transform.getState(sessionId).parked).toBe(false);
        expect(transformCalls).toBe(1);
    });
});

describe("prepareRustMemoryAuthority mixed restore", () => {
    it("resumes a schema-57 DRAINING restart through the real prepare path", async () => {
        const db = makeDb();
        const projectPath = "git:schema-57-restart";
        const projectRoot = "/worktrees/schema-57-restart";
        db.exec(`
            DROP TABLE mirror_live_staging;
            DROP TABLE mirror_resnapshot_state;
            DROP TABLE mirror_live_memory_rows;
            DELETE FROM schema_migrations WHERE version IN (58, 59, 60, 61, 62, 63);
        `);
        withPrivilegedWriter(db, () => {
            db.prepare(
                "INSERT INTO memories (id, project_path, category, content, normalized_hash, first_seen_at, created_at, updated_at, last_seen_at) VALUES (9395, ?, 'CONFIG_VALUES', 'drive model', 'same-hash', 0, 0, 0, 0)",
            ).run(projectPath);
            db.prepare(
                "INSERT INTO mirror_identity(domain, module_project, module_row_id, context_row_id) VALUES ('memories', '/legacy', 100, 9395)",
            ).run();
            db.prepare(
                "INSERT INTO mirror_cursors(domain, cursor, updated_at) VALUES ('memories', 20, 0)",
            ).run();
        });
        runMigrations(db);
        db.prepare(
            "UPDATE mirror_resnapshot_state SET status = 'resnapshotting' WHERE domain = 'memories'",
        ).run();
        db.prepare(
            "INSERT INTO mirror_live_staging VALUES ('abandoned', '/stale', 1, 'CONSTRAINTS', 'stale', NULL)",
        ).run();

        const calls: Array<{ liveOnly?: boolean; cursor: number }> = [];
        const statuses = new Map<string, AuthorityStatus | null>([
            [
                "memories",
                {
                    context_store_uuid: "store",
                    project: projectPath,
                    domain: "memories",
                    state: "DRAINING",
                    generation: 3,
                    captured_upper_bound: 21,
                    coordinator_token: "restart-token",
                },
            ],
            [
                "notes",
                {
                    context_store_uuid: "store",
                    project: projectPath,
                    domain: "notes",
                    state: "TS",
                    generation: 1,
                },
            ],
        ]);
        const memoryRow = (id: number, sourceProject: string) => ({
            id,
            project_path: sourceProject,
            category: "CONFIG_VALUES",
            content: "drive model",
            normalized_hash: "same-hash",
            status: "active",
        });
        const module: RustModeModuleClient = {
            call: async () => ({ ok: true }),
            authorityStatus: async (args) => ({ authority: statuses.get(args.domain) ?? null }),
            authorityPrepare: async () => {
                throw new Error("prepare should not run during DRAINING recovery");
            },
            authoritySeed: async () => ({ seeded: 0 }),
            authorityDrain: async (args) => {
                if (args.action === "finish") {
                    statuses.set("memories", {
                        context_store_uuid: "store",
                        project: projectPath,
                        domain: "memories",
                        state: "TS",
                        generation: 4,
                    });
                }
                return {
                    authority: {
                        context_store_uuid: "store",
                        project: projectPath,
                        domain: "memories",
                        state: args.action === "finish" ? "TS" : "DRAINING",
                        generation: args.action === "finish" ? 4 : 3,
                        captured_upper_bound: 21,
                        coordinator_token: "restart-token",
                    },
                };
            },
            mirrorPull: async (args) => {
                calls.push({ liveOnly: args.live_only, cursor: args.cursor });
                return args.live_only
                    ? {
                          page: {
                              domain: "memories",
                              cursor: 0,
                              next_cursor: 200,
                              has_more: false,
                              rows: [
                                  {
                                      feed_seq: 0,
                                      domain: "memories",
                                      op: "insert",
                                      module_row_id: 200,
                                      full_row_snapshot: memoryRow(200, projectPath),
                                      content_hash: "same-hash",
                                  },
                              ],
                          },
                      }
                    : {
                          page: {
                              domain: "memories",
                              cursor: args.cursor,
                              next_cursor: 21,
                              has_more: false,
                              rows: [
                                  {
                                      feed_seq: 21,
                                      domain: "memories",
                                      op: "tombstone",
                                      module_row_id: 100,
                                      full_row_snapshot: memoryRow(100, "/legacy"),
                                      content_hash: "same-hash",
                                  },
                              ],
                          },
                      };
            },
        };
        const state = {
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
            memoryAuthorityProject: null as string | null,
            memoryAuthorityRoot: null as string | null,
            memoryAuthorityReady: false,
        };

        await __rustModeTransformTest.prepareRustMemoryAuthority({
            db,
            module,
            projectPath,
            projectRoot,
            state,
        });

        expect(calls.map((call) => call.liveOnly)).toEqual([true, undefined]);
        expect(
            db.prepare("SELECT cursor FROM mirror_cursors WHERE domain = 'memories'").get(),
        ).toEqual({
            cursor: 21,
        });
        expect(db.prepare("SELECT id FROM memories WHERE id = 9395").get()).toEqual({ id: 9395 });
        expect(db.prepare("SELECT status FROM mirror_resnapshot_state").get()).toEqual({
            status: "complete",
        });
        expect(db.prepare("SELECT COUNT(*) AS count FROM mirror_live_staging").get()).toEqual({
            count: 0,
        });
        expect(state.memoryAuthorityReady).toBe(true);
    });

    it("reconciles remaining MODULE domains after a DRAINING resume before tools open", async () => {
        const db = makeDb();
        const projectPath = "git:mixed-restore";
        const projectRoot = "/worktrees/mixed-restore";
        const authorityRoots: string[] = [];
        const statuses = new Map<string, AuthorityStatus | null>([
            [
                "memories",
                {
                    context_store_uuid: "store",
                    project: projectPath,
                    domain: "memories",
                    state: "DRAINING",
                    generation: 3,
                    coordinator_token: "tok-a",
                    captured_upper_bound: 0,
                },
            ],
            [
                "notes",
                {
                    context_store_uuid: "store",
                    project: projectPath,
                    domain: "notes",
                    state: "MODULE",
                    generation: 2,
                },
            ],
        ]);
        const module: RustModeModuleClient = {
            call: async () => ({ ok: true }),
            authorityStatus: async (args) => {
                authorityRoots.push(String(args.projectRoot));
                return { authority: statuses.get(args.domain) ?? null };
            },
            authorityPrepare: async () => {
                throw new Error("prepare should not run on mixed DRAINING resume");
            },
            authoritySeed: async () => ({ seeded: 0 }),
            authorityDrain: async (args) => {
                authorityRoots.push(String(args.projectRoot));
                if (args.action === "begin") {
                    return {
                        authority: {
                            context_store_uuid: "store",
                            project: projectPath,
                            domain: "memories",
                            state: "DRAINING",
                            generation: 3,
                            coordinator_token: "tok-a",
                            captured_upper_bound: 0,
                        },
                    };
                }
                if (args.action === "finish") {
                    statuses.set("memories", {
                        context_store_uuid: "store",
                        project: projectPath,
                        domain: "memories",
                        state: "TS",
                        generation: 4,
                    });
                    return {
                        authority: {
                            context_store_uuid: "store",
                            project: projectPath,
                            domain: "memories",
                            state: "TS",
                            generation: 4,
                            coordinator_token: "tok-a",
                        },
                    };
                }
                return {
                    authority: {
                        context_store_uuid: "store",
                        project: projectPath,
                        domain: "memories",
                        state: "DRAINING",
                        generation: 3,
                        coordinator_token: "tok-a",
                    },
                };
            },
            mirrorPull: async (args) => {
                authorityRoots.push(String(args.projectRoot));
                return {
                    page: {
                        domain: args.domain,
                        cursor: args.cursor,
                        next_cursor: args.cursor,
                        has_more: false,
                        rows: [],
                    },
                };
            },
        };
        const state = {
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
            memoryAuthorityProject: null as string | null,
            memoryAuthorityRoot: null as string | null,
            memoryAuthorityReady: false,
        };
        const preparedProjects: string[] = [];
        await __rustModeTransformTest.prepareRustMemoryAuthority({
            db,
            module,
            projectPath,
            projectRoot,
            state,
            onProjectPrepared: (prepared) => preparedProjects.push(prepared),
        });
        expect(state.memoryAuthorityReady).toBe(true);
        // Hosts hang per-project services (the smart-note evaluator bridge) off this
        // callback, so it must fire with the RESOLVED project — a session that resolves
        // a project other than the plugin's launch directory still gets its bridge.
        expect(preparedProjects).toEqual([projectPath]);
        expect(authorityRoots.length).toBeGreaterThan(0);
        expect(authorityRoots.every((root) => root === projectRoot)).toBe(true);
        expect(getAuthorityManagedMarker(db, projectPath)).not.toBeNull();
        statuses.set("memories", {
            context_store_uuid: "store",
            project: projectPath,
            domain: "memories",
            state: "MODULE",
            generation: 4,
        });
        const secondRoot = "/worktrees/mixed-restore-two";
        await __rustModeTransformTest.prepareRustMemoryAuthority({
            db,
            module,
            projectPath,
            projectRoot: secondRoot,
            state,
        });
        expect(authorityRoots).toContain(secondRoot);
        expect(state.memoryAuthorityRoot).toBe(secondRoot);

        expect(() =>
            db
                .prepare(
                    "INSERT INTO notes(type, status, content, project_path, session_id, created_at, updated_at) VALUES ('plain', 'active', 'blocked', ?, 's', 0, 0)",
                )
                .run(projectPath),
        ).toThrow("managed by the Rust module");
    });
});
