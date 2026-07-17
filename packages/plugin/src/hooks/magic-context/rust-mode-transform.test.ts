/// <reference types="bun-types" />

import { afterEach, describe, expect, it } from "bun:test";
import { runMigrations } from "../../features/magic-context/migrations";
import type { ContextDatabase } from "../../features/magic-context/storage";
import { initializeDatabase } from "../../features/magic-context/storage-db";
import { getOrCreateSessionMeta } from "../../features/magic-context/storage-meta";
import { createMessagesTransformHandler } from "../../plugin/messages-transform";
import { Database } from "../../shared/sqlite";
import { closeQuietly } from "../../shared/sqlite-helpers";
import { setRawMessageProvider } from "./read-session-chunk";
import { createRustModeTransform, type RustModeModuleClient } from "./rust-mode-transform";
import type { TransformDeps } from "./transform";
import { createTransform } from "./transform";
import type { MessageLike } from "./transform-operations";

const sessions: string[] = [];
const databases: ContextDatabase[] = [];
const unregisters: Array<() => void> = [];

afterEach(() => {
    for (const unregister of unregisters.splice(0)) unregister();
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
    };
}

function makeMeta(
    db: ContextDatabase,
    sessionId: string,
): ReturnType<typeof getOrCreateSessionMeta> {
    return getOrCreateSessionMeta(db, sessionId);
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

    it("re-pages every transform payload after need_full_sync", async () => {
        const sessionId = `rust-repage-${Date.now()}`;
        sessions.push(sessionId);
        const db = makeDb();
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
