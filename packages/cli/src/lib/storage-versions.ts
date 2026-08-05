/**
 * Stable storage-version probe for doctor output.
 *
 * Answers the two questions fence incidents require answering: "which schema is
 * context.db actually at" and "which schema fence does this binary carry". Before
 * this probe existed, answering them meant hand-rolled greps and raw SELECTs
 * against the DB. Field names are snake_case to mirror the `storage_versions`
 * block of the mc-module status envelope, so fleet probes parse one shape across
 * both surfaces.
 */
import {
    getPersistedSchemaVersion,
    LATEST_SUPPORTED_VERSION,
} from "@magic-context/core/features/magic-context/storage-db";
import type { Database as DatabaseType } from "@magic-context/core/shared/sqlite";

export interface StorageVersions {
    /** Persisted schema version of context.db (MAX of schema_migrations). */
    context_db_schema_version: number;
    /** Highest context.db schema version this CLI/plugin build supports. */
    plugin_supported_version: number;
}

/** Read both probe values from an already-open context.db. Read-only. */
export function readStorageVersions(db: DatabaseType): StorageVersions {
    return {
        context_db_schema_version: getPersistedSchemaVersion(db),
        plugin_supported_version: LATEST_SUPPORTED_VERSION,
    };
}

/** One-line rendering used by doctor output and diagnostics. */
export function formatStorageVersions(versions: StorageVersions): string {
    return (
        `Storage versions: context_db_schema_version=${versions.context_db_schema_version}, ` +
        `plugin_supported_version=${versions.plugin_supported_version}`
    );
}
