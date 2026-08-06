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
    type FailClosedBlockingProcess,
    formatFailClosedBlockingMessage,
} from "@magic-context/core/features/magic-context/fail-closed-block";
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

export interface StorageVersionFenceCheck {
    alarm: boolean;
    message: string;
}

export const STALE_BUILD_RESTART_INSTRUCTION =
    "Magic Context: plugin build is older than its database — restart OpenCode";

/**
 * Classify storage_versions for doctor. A database below this build's fence only
 * means migrations are pending; only a database above the fence strands a stale
 * long-running server and is therefore an alarm.
 */
export function checkStorageVersionFence(
    versions: StorageVersions,
    options: { blockingProcesses?: readonly FailClosedBlockingProcess[] } = {},
): StorageVersionFenceCheck {
    const {
        context_db_schema_version: databaseVersion,
        plugin_supported_version: supportedVersion,
    } = versions;
    if (databaseVersion > supportedVersion) {
        return {
            alarm: true,
            message:
                `Storage schema fence alarm: context.db is v${databaseVersion}, but this build supports through v${supportedVersion}. ` +
                `${STALE_BUILD_RESTART_INSTRUCTION}.`,
        };
    }
    if (databaseVersion < supportedVersion) {
        if (options.blockingProcesses && options.blockingProcesses.length > 0) {
            return {
                alarm: true,
                message: formatFailClosedBlockingMessage({
                    kind: "migration_guard",
                    persistedVersion: databaseVersion,
                    supportedVersion,
                    blockingProcesses: options.blockingProcesses,
                }),
            };
        }
        return {
            alarm: false,
            message: `Storage schema migrations pending: context.db is v${databaseVersion}; this build supports through v${supportedVersion}.`,
        };
    }
    return {
        alarm: false,
        message: `Storage schema fence: context.db and this build are both v${supportedVersion}.`,
    };
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
