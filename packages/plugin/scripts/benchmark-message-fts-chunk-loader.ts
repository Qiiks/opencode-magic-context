/// <reference types="bun-types" />

import { performance } from "node:perf_hooks";

import { MESSAGE_FTS_CHUNK_LOAD_SQL } from "../src/features/magic-context/compartment-chunk-embedding";
import { Database } from "../src/shared/sqlite";
import { closeQuietly } from "../src/shared/sqlite-helpers";

const ROW_COUNT = 200_000;
const START_ORDINAL = 190_001;
const END_ORDINAL = 190_100;
const SESSION_ID = "benchmark-session";
const SAMPLE_COUNT = 5;

function median(values: number[]): number {
    const sorted = [...values].sort((a, b) => a - b);
    return sorted[Math.floor(sorted.length / 2)] ?? 0;
}

function planDetails(db: Database, sql: string, params: unknown[]): string[] {
    return (
        db.prepare(`EXPLAIN QUERY PLAN ${sql}`).all(...params) as Array<{ detail: string }>
    ).map((row) => row.detail);
}

const db = new Database(":memory:");
try {
    db.exec(`
        CREATE VIRTUAL TABLE message_history_fts USING fts5(
            session_id UNINDEXED,
            message_ordinal UNINDEXED,
            message_id UNINDEXED,
            role,
            content,
            tokenize='porter unicode61'
        );
        CREATE TABLE message_fts_rowid_map (
            session_id TEXT NOT NULL,
            message_ordinal INTEGER NOT NULL,
            fts_rowid INTEGER NOT NULL,
            PRIMARY KEY(session_id, message_ordinal)
        );
    `);
    const insertFts = db.prepare(
        "INSERT INTO message_history_fts (session_id, message_ordinal, message_id, role, content) VALUES (?, ?, ?, ?, ?)",
    );
    const insertMap = db.prepare(
        "INSERT INTO message_fts_rowid_map (session_id, message_ordinal, fts_rowid) VALUES (?, ?, ?)",
    );
    const fatSuffix = ` ${"fat transcript payload ".repeat(48)}`;
    db.transaction(() => {
        for (let ordinal = 1; ordinal <= ROW_COUNT; ordinal += 1) {
            const result = insertFts.run(
                SESSION_ID,
                ordinal,
                `m-${ordinal}`,
                ordinal % 2 === 0 ? "assistant" : "user",
                `message ${ordinal}${fatSuffix}`,
            ) as { lastInsertRowid: number | bigint };
            insertMap.run(SESSION_ID, ordinal, Number(result.lastInsertRowid));
        }
    })();

    const oldSql = `SELECT message_ordinal AS messageOrdinal, role, content
                    FROM message_history_fts
                    WHERE session_id = ?
                      AND message_ordinal >= ?
                      AND message_ordinal <= ?
                      AND role IN ('user', 'assistant')
                    ORDER BY message_ordinal ASC`;
    const oldStatement = db.prepare(oldSql);
    const newStatement = db.prepare(MESSAGE_FTS_CHUNK_LOAD_SQL);
    const params = [SESSION_ID, START_ORDINAL, END_ORDINAL];

    oldStatement.all(...params);
    newStatement.all(...params);
    const oldSamples: number[] = [];
    const newSamples: number[] = [];
    for (let sample = 0; sample < SAMPLE_COUNT; sample += 1) {
        let started = performance.now();
        oldStatement.all(...params);
        oldSamples.push(performance.now() - started);
        started = performance.now();
        newStatement.all(...params);
        newSamples.push(performance.now() - started);
    }

    const oldMedian = median(oldSamples);
    const newMedian = median(newSamples);
    console.log(`rows=${ROW_COUNT} content_chars=${fatSuffix.length} span=${END_ORDINAL - START_ORDINAL + 1}`);
    console.log(`old_ms=${oldMedian.toFixed(3)} samples=${oldSamples.map((n) => n.toFixed(3)).join(",")}`);
    console.log(`new_ms=${newMedian.toFixed(3)} samples=${newSamples.map((n) => n.toFixed(3)).join(",")}`);
    console.log(`speedup=${(oldMedian / Math.max(newMedian, 0.000_001)).toFixed(1)}x`);
    console.log(`old_plan=${planDetails(db, oldSql, params).join(" | ")}`);
    console.log(`new_plan=${planDetails(db, MESSAGE_FTS_CHUNK_LOAD_SQL, params).join(" | ")}`);
} finally {
    closeQuietly(db);
}
