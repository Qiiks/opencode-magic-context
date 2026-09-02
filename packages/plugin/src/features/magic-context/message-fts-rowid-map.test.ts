/// <reference types="bun-types" />

import { afterEach, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { Database } from "../../shared/sqlite";
import { closeQuietly } from "../../shared/sqlite-helpers";
import {
    buildCanonicalChunkTextFromFts,
    MESSAGE_FTS_CHUNK_LOAD_SQL,
} from "./compartment-chunk-embedding";
import {
    backfillMessageFtsRowidMapBatch,
    getMessageFtsRowidMapBackfillProgress,
} from "./message-fts-rowid-map";
import { runMigrations } from "./migrations";
import { initializeDatabase } from "./storage-db";

const tempDirectories: string[] = [];

function createDb(path = ":memory:"): Database {
    const db = new Database(path);
    initializeDatabase(db);
    runMigrations(db);
    return db;
}

function insertLegacyFtsRows(db: Database, count: number, sessionId = "legacy"): void {
    const insert = db.prepare(
        "INSERT INTO message_history_fts (session_id, message_ordinal, message_id, role, content) VALUES (?, ?, ?, ?, ?)",
    );
    for (let ordinal = 1; ordinal <= count; ordinal += 1) {
        insert.run(
            sessionId,
            ordinal,
            `m${ordinal}`,
            ordinal % 2 === 0 ? "assistant" : "user",
            `fat legacy payload ${ordinal} ${"x".repeat(256)}`,
        );
    }
}

afterEach(() => {
    for (const directory of tempDirectories) {
        rmSync(directory, { recursive: true, force: true });
    }
    tempDirectories.length = 0;
});

describe("message FTS rowid map", () => {
    test("backfills bounded windows and resumes from the persisted watermark after restart", () => {
        const directory = mkdtempSync(join(tmpdir(), "message-fts-rowid-map-"));
        tempDirectories.push(directory);
        const path = join(directory, "context.db");
        let db = createDb(path);
        insertLegacyFtsRows(db, 5);

        expect(backfillMessageFtsRowidMapBatch(db, 2)).toEqual({
            processed: 2,
            watermarkRowid: 2,
            completed: false,
        });
        expect(
            db.prepare("SELECT message_ordinal, fts_rowid FROM message_fts_rowid_map ORDER BY message_ordinal").all(),
        ).toEqual([
            { message_ordinal: 1, fts_rowid: 1 },
            { message_ordinal: 2, fts_rowid: 2 },
        ]);
        closeQuietly(db);

        db = createDb(path);
        expect(getMessageFtsRowidMapBackfillProgress(db)).toMatchObject({
            watermarkRowid: 2,
            completed: false,
        });
        expect(backfillMessageFtsRowidMapBatch(db, 2)).toEqual({
            processed: 2,
            watermarkRowid: 4,
            completed: false,
        });
        expect(backfillMessageFtsRowidMapBatch(db, 2)).toEqual({
            processed: 1,
            watermarkRowid: 5,
            completed: true,
        });
        expect(
            db.prepare("SELECT COUNT(*) AS count FROM message_fts_rowid_map").get(),
        ).toEqual({ count: 5 });
        closeQuietly(db);
    });

    test("canonical loader is byte-identical on mapped data and defers unmapped spans", () => {
        const db = createDb();
        try {
            insertLegacyFtsRows(db, 3, "canonical");
            expect(buildCanonicalChunkTextFromFts(db, "canonical", 1, 3)).toBeNull();

            backfillMessageFtsRowidMapBatch(db, 500);
            expect(buildCanonicalChunkTextFromFts(db, "canonical", 1, 3)).toBe(
                `[1] U: fat legacy payload 1 ${"x".repeat(256)}\n` +
                    `[2] A: fat legacy payload 2 ${"x".repeat(256)}\n` +
                    `[3] U: fat legacy payload 3 ${"x".repeat(256)}`,
            );
        } finally {
            closeQuietly(db);
        }
    });

    test("chunk loader plan uses the map primary key and FTS rowid point lookups", () => {
        const db = createDb();
        try {
            insertLegacyFtsRows(db, 3, "plan");
            backfillMessageFtsRowidMapBatch(db, 500);
            const plan = db
                .prepare(`EXPLAIN QUERY PLAN ${MESSAGE_FTS_CHUNK_LOAD_SQL}`)
                .all("plan", 1, 3) as Array<{ detail: string }>;
            const details = plan.map((row) => row.detail);
            console.log(`[message-fts-rowid-map] chunk loader plan: ${details.join(" | ")}`);

            expect(details.some((detail) => detail.includes("message_fts_rowid_map"))).toBe(true);
            expect(details.some((detail) => detail.includes("fts") && detail.includes("INDEX 0:="))).toBe(
                true,
            );
            expect(
                details.some((detail) => /^SCAN (?:message_history_fts|fts) VIRTUAL TABLE INDEX 0:$/.test(detail)),
            ).toBe(false);
        } finally {
            closeQuietly(db);
        }
    });
});
