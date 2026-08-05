import { describe, expect, it } from "bun:test";
import {
    initializeDatabase,
    LATEST_SUPPORTED_VERSION,
    runMigrations,
} from "@magic-context/core/features/magic-context/storage";
import { Database } from "@magic-context/core/shared/sqlite";
import { formatStorageVersions, readStorageVersions } from "./storage-versions";

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
