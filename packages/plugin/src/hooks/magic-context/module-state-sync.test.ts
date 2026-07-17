/// <reference types="bun-types" />

import { afterEach, describe, expect, it } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { appendCompartments } from "../../features/magic-context/compartment-storage";
import { runMigrations } from "../../features/magic-context/migrations";
import { getCompartments } from "../../features/magic-context/storage";
import { initializeDatabase } from "../../features/magic-context/storage-db";
import { Database } from "../../shared/sqlite";
import { closeQuietly } from "../../shared/sqlite-helpers";
import { mirrorModuleCompartments, syncModuleState } from "./module-state-sync";
import { resolveOrdinalsForModule } from "./module-wire";
import { closeReadOnlySessionDb } from "./read-session-db";

const databases: Database[] = [];
const tempDirs: string[] = [];
const originalXdgDataHome = process.env.XDG_DATA_HOME;

afterEach(() => {
    for (const db of databases.splice(0)) closeQuietly(db);
    closeReadOnlySessionDb();
    if (originalXdgDataHome === undefined) delete process.env.XDG_DATA_HOME;
    else process.env.XDG_DATA_HOME = originalXdgDataHome;
    for (const dir of tempDirs.splice(0)) rmSync(dir, { recursive: true, force: true });
});

function useTempDataHome(prefix: string): void {
    const dir = mkdtempSync(join(tmpdir(), prefix));
    tempDirs.push(dir);
    process.env.XDG_DATA_HOME = dir;
}

function createOpenCodeDb(
    sessionId: string,
    messages: Array<{ id: string; role: string; summary?: boolean }>,
): void {
    const dbPath = join(process.env.XDG_DATA_HOME ?? "", "opencode", "opencode.db");
    mkdirSync(dirname(dbPath), { recursive: true });
    const db = new Database(dbPath);
    try {
        db.exec(`
            CREATE TABLE message (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL,
                data TEXT NOT NULL
            );
            CREATE TABLE part (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                message_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL,
                data TEXT NOT NULL
            );
        `);
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
                    summary: message.summary === true ? true : undefined,
                    finish: message.summary === true ? "stop" : undefined,
                }),
            );
            insertPart.run(
                message.id,
                sessionId,
                timestamp,
                timestamp,
                JSON.stringify({ type: "text", text: message.id }),
            );
        });
    } finally {
        closeQuietly(db);
    }
}

function createContextDb(): Database {
    const db = new Database(":memory:");
    databases.push(db);
    initializeDatabase(db);
    runMigrations(db);
    return db;
}

function syncState(generation = 1): {
    shadowGeneration: number;
    lastAckedSeq: number;
    lastAckedWatermarks: null;
    idOrdinalMemoGeneration: number;
    idOrdinalMemo: Map<string, number>;
    seedPassPending: boolean;
} {
    return {
        shadowGeneration: generation,
        lastAckedSeq: 0,
        lastAckedWatermarks: null,
        idOrdinalMemoGeneration: generation,
        idOrdinalMemo: new Map(),
        seedPassPending: true,
    };
}

function wireMessage(
    sessionId: string,
    id: string,
): {
    info: { id: string; role: string; sessionID: string };
    parts: Array<{ type: string; text: string }>;
} {
    return {
        info: { id, role: "user", sessionID: sessionId },
        parts: [{ type: "text", text: id }],
    };
}

describe("module compartment ordinal serialization", () => {
    it("uses canonical ordinals when stored boundaries include a summary row", async () => {
        useTempDataHome("module-state-sync-ordinal-basis-");
        const sessionId = "ses-ordinal-basis";
        createOpenCodeDb(sessionId, [
            { id: "m1", role: "user" },
            { id: "summary", role: "assistant", summary: true },
            { id: "m2", role: "assistant" },
            { id: "m3", role: "user" },
        ]);
        const db = createContextDb();
        appendCompartments(db, sessionId, [
            {
                sequence: 0,
                startMessage: 3,
                endMessage: 4,
                startMessageId: "m2",
                endMessageId: "m3",
                title: "After summary",
                content: "content",
            },
        ]);

        const state = syncState(7);
        const wire = await resolveOrdinalsForModule({
            sessionId,
            messages: [wireMessage(sessionId, "m2")],
            generation: state.shadowGeneration,
            memoGeneration: state.idOrdinalMemoGeneration,
            memo: state.idOrdinalMemo,
        });
        expect(wire).toEqual(
            expect.objectContaining({
                ok: true,
                annotatedInput: [expect.objectContaining({ absolute_ordinal: 2 })],
            }),
        );
        expect(state.idOrdinalMemo.get("m2")).toBe(2);

        const calls: unknown[] = [];
        await syncModuleState({
            client: {
                async call(args) {
                    calls.push(args.body);
                    return { result: { shadow_seq: 1 } };
                },
            },
            state,
            pass: { db, sessionId, nowMs: 1 },
            projectRoot: "/tmp/project",
            force: true,
        });

        const body = calls[0] as {
            compartments: Array<{ start_message: number; end_message: number }>;
        };
        expect(body.compartments).toEqual([
            expect.objectContaining({ start_message: 2, end_message: 3 }),
        ]);
    });

    it("keeps canonical ordinal drift fail-loud when the wire resolver finds a conflict", async () => {
        useTempDataHome("module-state-sync-ordinal-drift-");
        const sessionId = "ses-ordinal-drift";
        createOpenCodeDb(sessionId, [
            { id: "m1", role: "user" },
            { id: "summary", role: "assistant", summary: true },
            { id: "m2", role: "user" },
        ]);
        const state = syncState(3);
        state.idOrdinalMemo.set("m2", 3);

        const result = await resolveOrdinalsForModule({
            sessionId,
            messages: [wireMessage(sessionId, "m2")],
            generation: state.shadowGeneration,
            memoGeneration: state.idOrdinalMemoGeneration,
            memo: state.idOrdinalMemo,
            memoStoredCount: 3,
            memoCanonicalCount: 0,
        });

        expect(result).toEqual(expect.objectContaining({ ok: false, reason: "mismatch" }));
    });

    it("preserves stored ordinals when a session has no summary rows", async () => {
        useTempDataHome("module-state-sync-no-summary-");
        const sessionId = "ses-no-summary";
        createOpenCodeDb(sessionId, [
            { id: "m1", role: "user" },
            { id: "m2", role: "assistant" },
        ]);
        const db = createContextDb();
        appendCompartments(db, sessionId, [
            {
                sequence: 0,
                startMessage: 1,
                endMessage: 2,
                startMessageId: "m1",
                endMessageId: "m2",
                title: "No summary",
                content: "content",
            },
        ]);

        const calls: unknown[] = [];
        await syncModuleState({
            client: {
                async call(args) {
                    calls.push(args.body);
                    return { result: { shadow_seq: 1 } };
                },
            },
            state: syncState(),
            pass: { db, sessionId, nowMs: 1 },
            projectRoot: "/tmp/project",
            force: true,
        });

        const body = calls[0] as {
            compartments: Array<{ start_message: number; end_message: number }>;
        };
        expect(body.compartments).toEqual([
            expect.objectContaining({ start_message: 1, end_message: 2 }),
        ]);
    });
});

describe("module compartment mirror-back", () => {
    it("copies rows after the local watermark idempotently", async () => {
        const db = new Database(":memory:");
        databases.push(db);
        initializeDatabase(db);
        runMigrations(db);
        const calls: number[] = [];
        const reader = {
            async getCompartmentsAfter(_sessionId: string, afterSequence: number) {
                calls.push(afterSequence);
                return afterSequence < 2
                    ? {
                          max_sequence: 2,
                          compartments: [
                              {
                                  sequence: 1,
                                  start_message: 1,
                                  end_message: 2,
                                  start_message_id: "m1#0",
                                  end_message_id: "m2#0",
                                  title: "First",
                                  content: "first content",
                                  created_at: 10,
                              },
                              {
                                  sequence: 2,
                                  start_message: 3,
                                  end_message: 4,
                                  start_message_id: "m3#0",
                                  end_message_id: "m4#0",
                                  title: "Second",
                                  content: "second content",
                                  created_at: 20,
                              },
                          ],
                      }
                    : { max_sequence: 2, compartments: [] };
            },
        };

        await mirrorModuleCompartments({ db, sessionId: "ses-mirror", reader });
        await mirrorModuleCompartments({ db, sessionId: "ses-mirror", reader });

        expect(calls).toEqual([-1, 2]);
        expect(getCompartments(db, "ses-mirror").map((row) => row.sequence)).toEqual([1, 2]);
    });
});
