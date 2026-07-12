import { existsSync, writeFileSync } from "node:fs";
import { pathToFileURL } from "node:url";
import {
    getPersistedSchemaVersion,
    LATEST_SUPPORTED_VERSION,
} from "@magic-context/core/features/magic-context/storage-db";
import type { Database as DatabaseType } from "@magic-context/core/shared/sqlite";
import { Database } from "@magic-context/core/shared/sqlite";

export class UnsupportedSchemaVersionError extends Error {
    readonly path: string;
    readonly persistedVersion: number;
    readonly supportedVersion: number;

    constructor(path: string, persistedVersion: number, supportedVersion: number) {
        super(
            `Refusing to open ${path}: database schema v${persistedVersion} is newer than this CLI supports (max v${supportedVersion}). Update Magic Context before using this database.`,
        );
        this.name = "UnsupportedSchemaVersionError";
        this.path = path;
        this.persistedVersion = persistedVersion;
        this.supportedVersion = supportedVersion;
    }
}

/**
 * Opens an existing SQLite file without silently creating an empty replacement.
 * Callers must treat null as a graceful missing-database path.
 */
export function openExistingDatabase(
    path: string,
    options: { readonly: boolean },
): DatabaseType | null {
    if (!existsSync(path)) return null;
    if (options.readonly) return new Database(path, { readonly: true });

    // SQLite's URI mode=rw omits SQLITE_OPEN_CREATE. The existence check gives
    // callers a graceful null result, while the URI also closes the race where
    // the file disappears between that check and the constructor.
    const uri = pathToFileURL(path);
    uri.searchParams.set("mode", "rw");
    return new Database(uri.href);
}

/**
 * Applies the shared schema fence immediately after opening context.db. No query
 * or migration write may run until this check accepts the persisted version.
 */
export function openExistingContextDatabase(
    path: string,
    options: { readonly: boolean },
): DatabaseType | null {
    const db = openExistingDatabase(path, options);
    if (db === null) return null;

    try {
        const persistedVersion = getPersistedSchemaVersion(db);
        if (persistedVersion > LATEST_SUPPORTED_VERSION) {
            throw new UnsupportedSchemaVersionError(
                path,
                persistedVersion,
                LATEST_SUPPORTED_VERSION,
            );
        }
        return db;
    } catch (error) {
        db.close();
        throw error;
    }
}

/** Create a consistent SQLite snapshot, including committed WAL contents. */
export async function backupDatabaseSnapshot(db: DatabaseType, destination: string): Promise<void> {
    const serializable = db as DatabaseType & { serialize?: () => Uint8Array };
    if (typeof serializable.serialize === "function") {
        writeFileSync(destination, serializable.serialize(), { flag: "wx" });
        return;
    }

    const moduleName = "node:" + "sqlite";
    const sqlite = (await import(moduleName)) as {
        backup?: (source: unknown, path: string) => Promise<void>;
    };
    if (typeof sqlite.backup !== "function") {
        throw new Error("The active SQLite runtime does not provide a snapshot backup API");
    }
    await sqlite.backup(db, destination);
}
