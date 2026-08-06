/// <reference types="bun-types" />

import { describe, expect, it } from "bun:test";
import {
    initializeDatabase,
    LATEST_SUPPORTED_VERSION,
    runMigrations,
} from "@magic-context/core/features/magic-context/storage";
import { Database } from "@magic-context/core/shared/sqlite";
import {
    checkStorageVersionFence,
    formatStorageVersions,
    readStorageVersions,
    STALE_BUILD_RESTART_INSTRUCTION,
} from "./storage-versions";

describe("storage versions probe", () => {
    it("reads the live schema version and the binary fence from a fully migrated DB", () => {
        const db = new Database(":memory:");
        try {
            initializeDatabase(db);
            runMigrations(db);

            const versions = readStorageVersions(db);

            // A fully migrated DB sits exactly at the fence: the live MAX query and
            // the compile-time constant must agree, and the probe reports both.
            expect(versions.context_db_schema_version).toBe(LATEST_SUPPORTED_VERSION);
            expect(versions.plugin_supported_version).toBe(LATEST_SUPPORTED_VERSION);
            expect(formatStorageVersions(versions)).toBe(
                `Storage versions: context_db_schema_version=${LATEST_SUPPORTED_VERSION}, ` +
                    `plugin_supported_version=${LATEST_SUPPORTED_VERSION}`,
            );
        } finally {
            db.close();
        }
    });

    it("follows an older live DB version while the fence stays put", () => {
        const db = new Database(":memory:");
        try {
            // A DB last touched by an older binary: schema_migrations stops at 50.
            db.exec("CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY)");
            db.prepare("INSERT INTO schema_migrations (version) VALUES (?)").run(50);

            const versions = readStorageVersions(db);

            expect(versions.context_db_schema_version).toBe(50);
            expect(versions.plugin_supported_version).toBe(LATEST_SUPPORTED_VERSION);
            expect(formatStorageVersions(versions)).toBe(
                "Storage versions: context_db_schema_version=50, " +
                    `plugin_supported_version=${LATEST_SUPPORTED_VERSION}`,
            );
        } finally {
            db.close();
        }
    });

    it("reports 0 for a DB without a migrations table", () => {
        const db = new Database(":memory:");
        try {
            const versions = readStorageVersions(db);
            expect(versions.context_db_schema_version).toBe(0);
            expect(versions.plugin_supported_version).toBe(LATEST_SUPPORTED_VERSION);
        } finally {
            db.close();
        }
    });
});

describe("checkStorageVersionFence", () => {
    it("alarms only when the database is newer than the build", () => {
        const result = checkStorageVersionFence({
            context_db_schema_version: 73,
            plugin_supported_version: 72,
        });

        expect(result.alarm).toBe(true);
        expect(result.message).toContain(STALE_BUILD_RESTART_INSTRUCTION);
    });

    it("reports migrations pending without alarming when the database is older", () => {
        const result = checkStorageVersionFence({
            context_db_schema_version: 71,
            plugin_supported_version: 72,
        });
        expect(result).toEqual({
            alarm: false,
            message:
                "Storage schema migrations pending: context.db is v71; this build supports through v72.",
        });
    });

    it("prints the actionable migration guard when live OpenCode servers block it", () => {
        const result = checkStorageVersionFence(
            {
                context_db_schema_version: 73,
                plugin_supported_version: 74,
            },
            {
                blockingProcesses: [
                    { harness: "OpenCode server", pid: 5736 },
                    { harness: "OpenCode server", pid: 5736 },
                ],
            },
        );

        expect(result.alarm).toBe(true);
        expect(result.message).toContain("OpenCode server (PID 5736)");
        expect(result.message).toContain("an older Magic Context build");
        expect(result.message).toContain("shut it down and retry");
    });
});
