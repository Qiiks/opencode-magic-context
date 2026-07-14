import { afterEach, describe, expect, it } from "bun:test";
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { LATEST_SUPPORTED_VERSION } from "@magic-context/core/features/magic-context/storage-db";
import { Database } from "@magic-context/core/shared/sqlite";
import { openExistingContextDatabase, UnsupportedSchemaVersionError } from "./database-access";

const tempDirs: string[] = [];

function tempDir(): string {
    const path = mkdtempSync(join(tmpdir(), "mc-cli-db-access-"));
    tempDirs.push(path);
    return path;
}

function createVersionedDatabase(path: string, version: number): void {
    const db = new Database(path);
    db.exec("CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY)");
    db.prepare("INSERT INTO schema_migrations (version) VALUES (?)").run(version);
    db.close();
}

afterEach(() => {
    for (const path of tempDirs.splice(0)) rmSync(path, { recursive: true, force: true });
});

describe("CLI context database access", () => {
    it("rejects a database newer than the shared supported schema", () => {
        const path = join(tempDir(), "context.db");
        // Derive from the live fence so a routine schema bump cannot stale this fixture.
        const newerVersion = LATEST_SUPPORTED_VERSION + 1;
        createVersionedDatabase(path, newerVersion);

        expect(() => openExistingContextDatabase(path, { readonly: false })).toThrow(
            UnsupportedSchemaVersionError,
        );
        expect(() => openExistingContextDatabase(path, { readonly: true })).toThrow(
            `database schema v${newerVersion} is newer than this CLI supports (max v${LATEST_SUPPORTED_VERSION})`,
        );
    });

    it("does not create a database when context.db is missing", () => {
        const path = join(tempDir(), "context.db");

        expect(openExistingContextDatabase(path, { readonly: false })).toBeNull();
        expect(existsSync(path)).toBe(false);
    });

    it("opens the current supported schema normally for reads and migration writes", () => {
        const path = join(tempDir(), "context.db");
        createVersionedDatabase(path, LATEST_SUPPORTED_VERSION);

        const db = openExistingContextDatabase(path, { readonly: false });
        expect(db).not.toBeNull();
        db?.exec("CREATE TABLE migration_probe (id INTEGER PRIMARY KEY)");
        db?.close();

        const readonlyDb = openExistingContextDatabase(path, { readonly: true });
        const table = readonlyDb
            ?.prepare(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'migration_probe'",
            )
            .get() as { name?: string } | undefined;
        expect(table?.name).toBe("migration_probe");
        readonlyDb?.close();
    });
});
