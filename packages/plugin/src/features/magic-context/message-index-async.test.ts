/// <reference types="bun-types" />

import { afterEach, beforeEach, describe, expect, it } from "bun:test";
import type { RawMessage } from "../../hooks/magic-context/read-session-raw";
import { Database } from "../../shared/sqlite";
import { closeQuietly } from "../../shared/sqlite-helpers";
import {
    __resetMessageIndexAsyncForTests,
    clearSessionTracking,
    isSessionReconciled,
    scheduleClearAndReindex,
    scheduleIncrementalIndex,
    scheduleReconciliation,
} from "./message-index-async";
import { initializeDatabase } from "./storage-db";

function createTestDb(): Database {
    const db = new Database(":memory:");
    initializeDatabase(db);
    return db;
}

function message(id: string, ordinal: number, text: string): RawMessage {
    return {
        id,
        ordinal,
        role: "user",
        parts: [{ type: "text", text }],
    };
}

function wait(ms = 0): Promise<void> {
    return new Promise((resolve) => setTimeout(resolve, ms));
}

function countRows(db: Database, sessionId: string): number {
    const row = db
        .prepare("SELECT COUNT(*) AS count FROM message_history_fts WHERE session_id = ?")
        .get(sessionId) as { count?: number } | null;
    return typeof row?.count === "number" ? row.count : 0;
}

function countMessageRows(db: Database, sessionId: string, messageId: string): number {
    const row = db
        .prepare(
            "SELECT COUNT(*) AS count FROM message_history_fts WHERE session_id = ? AND message_id = ?",
        )
        .get(sessionId, messageId) as { count?: number } | null;
    return typeof row?.count === "number" ? row.count : 0;
}

function searchMessageIds(db: Database, sessionId: string, ftsQuery: string): string[] {
    return (
        db
            .prepare(
                "SELECT message_id FROM message_history_fts WHERE session_id = ? AND message_history_fts MATCH ? ORDER BY bm25(message_history_fts), CAST(message_ordinal AS INTEGER) ASC",
            )
            .all(sessionId, ftsQuery) as Array<{ message_id: string }>
    ).map((row) => row.message_id);
}

describe("message-index-async", () => {
    let db: Database;

    beforeEach(() => {
        __resetMessageIndexAsyncForTests();
        db = createTestDb();
    });

    afterEach(() => {
        closeQuietly(db);
        __resetMessageIndexAsyncForTests();
    });

    it("dedupes concurrent reconciliation schedules for one session", async () => {
        const messages = [message("m-1", 1, "alpha")];
        let reads = 0;

        scheduleReconciliation(db, "ses-async", () => {
            reads++;
            return messages;
        });
        scheduleReconciliation(db, "ses-async", () => {
            reads++;
            return messages;
        });

        await wait(20);

        expect(reads).toBe(1);
        expect(countRows(db, "ses-async")).toBe(1);
        expect(isSessionReconciled("ses-async")).toBe(true);
    });

    it("does not double-insert when incremental indexing overlaps reconciliation", async () => {
        const messages = [message("m-1", 1, "alpha overlap")];
        scheduleReconciliation(db, "ses-overlap", () => messages);
        scheduleIncrementalIndex(db, "ses-overlap", "m-1", () => messages[0] ?? null);

        await wait(140);

        expect(countMessageRows(db, "ses-overlap", "m-1")).toBe(1);
    });

    it("reconciles a failed incremental hole even after a later incremental success advanced the watermark", async () => {
        const originalPrepare = db.prepare.bind(db);
        let failMessageId: string | null = "m-2";
        (db as unknown as { prepare: typeof db.prepare }).prepare = ((sql: string) => {
            const stmt = originalPrepare(sql);
            if (sql.startsWith("INSERT INTO message_history_fts")) {
                const run = stmt.run.bind(stmt);
                (stmt as unknown as { run: typeof stmt.run }).run = ((...args: unknown[]) => {
                    if (failMessageId !== null && args[2] === failMessageId) {
                        throw new Error("synthetic incremental failure");
                    }
                    return run(...(args as Parameters<typeof stmt.run>));
                }) as typeof stmt.run;
            }
            return stmt;
        }) as typeof db.prepare;

        const fullHistory = [
            message("m-1", 1, "alpha indexed first"),
            message("m-2", 2, "beta hole should come back"),
            message("m-3", 3, "gamma later incremental succeeds"),
        ];
        scheduleReconciliation(db, "ses-hole", () => [fullHistory[0]!]);
        await wait(20);
        expect(isSessionReconciled("ses-hole")).toBe(true);

        scheduleIncrementalIndex(db, "ses-hole", "m-2", () => fullHistory[1] ?? null);
        await wait(140);
        expect(countMessageRows(db, "ses-hole", "m-2")).toBe(0);
        expect(isSessionReconciled("ses-hole")).toBe(false);

        failMessageId = null;
        scheduleIncrementalIndex(db, "ses-hole", "m-3", () => fullHistory[2] ?? null);
        await wait(140);
        expect(countMessageRows(db, "ses-hole", "m-3")).toBe(1);

        scheduleReconciliation(db, "ses-hole", () => fullHistory);
        await wait(20);

        expect(searchMessageIds(db, "ses-hole", "beta")).toEqual(["m-2"]);
        expect(countMessageRows(db, "ses-hole", "m-3")).toBe(1);
        expect(isSessionReconciled("ses-hole")).toBe(true);
    });

    it("clears and rebuilds after a removed message", async () => {
        const first = [message("m-1", 1, "old"), message("m-2", 2, "keep")];
        scheduleReconciliation(db, "ses-clear", () => first);
        await wait(20);

        const rebuilt = [message("m-2", 1, "keep")];
        scheduleClearAndReindex(db, "ses-clear", () => rebuilt);
        await wait(20);

        expect(countMessageRows(db, "ses-clear", "m-1")).toBe(0);
        expect(countMessageRows(db, "ses-clear", "m-2")).toBe(1);
        expect(isSessionReconciled("ses-clear")).toBe(true);
    });

    it("catches indexing errors without propagating", async () => {
        expect(() =>
            scheduleReconciliation(db, "ses-error", () => {
                throw new Error("boom");
            }),
        ).not.toThrow();

        await wait(20);
        expect(isSessionReconciled("ses-error")).toBe(false);
    });

    it("clearSessionTracking releases module state", async () => {
        scheduleReconciliation(db, "ses-track", () => [message("m-1", 1, "alpha")]);
        await wait(20);
        expect(isSessionReconciled("ses-track")).toBe(true);

        clearSessionTracking("ses-track");

        expect(isSessionReconciled("ses-track")).toBe(false);
    });
});
