/// <reference types="bun-types" />

import { describe, expect, test } from "bun:test";
import { Database } from "../../shared/sqlite";
import { closeQuietly } from "../../shared/sqlite-helpers";
import { LATEST_MIGRATION_VERSION, runMigrations } from "./migrations";
import { initializeDatabase, LATEST_SUPPORTED_VERSION } from "./storage-db";

function seedAppliedVersions(db: Database, through: number): void {
    db.exec(`
        CREATE TABLE schema_migrations (
            version INTEGER PRIMARY KEY,
            description TEXT NOT NULL,
            applied_at INTEGER NOT NULL
        );
    `);
    const insert = db.prepare(
        "INSERT INTO schema_migrations (version, description, applied_at) VALUES (?, ?, ?)",
    );
    for (let version = 1; version <= through; version += 1) {
        insert.run(version, `seed v${version}`, Date.now());
    }
}

function columnNames(db: Database): string[] {
    return (db.prepare("PRAGMA table_info(session_meta)").all() as Array<{ name: string }>).map(
        (column) => column.name,
    );
}

describe("migration v81: Channel-1 last-fire dampening state", () => {
    test("fresh databases include both last-fire fields and align the schema fence", () => {
        const db = new Database(":memory:");
        try {
            initializeDatabase(db);
            runMigrations(db);
            expect(columnNames(db)).toEqual(
                expect.arrayContaining(["channel1_last_fire_level", "channel1_last_fire_ordinal"]),
            );
            expect(LATEST_SUPPORTED_VERSION).toBe(81);
            expect(LATEST_SUPPORTED_VERSION).toBe(LATEST_MIGRATION_VERSION);
        } finally {
            closeQuietly(db);
        }
    });

    test("upgrades a pre-v81 session_meta table idempotently", () => {
        const db = new Database(":memory:");
        try {
            db.exec("CREATE TABLE session_meta (session_id TEXT PRIMARY KEY)");
            seedAppliedVersions(db, 80);
            runMigrations(db);
            runMigrations(db);
            expect(columnNames(db)).toEqual(
                expect.arrayContaining(["channel1_last_fire_level", "channel1_last_fire_ordinal"]),
            );
            expect(
                db
                    .prepare("SELECT COUNT(*) AS count FROM schema_migrations WHERE version = 81")
                    .get(),
            ).toEqual({ count: 1 });
        } finally {
            closeQuietly(db);
        }
    });
});
