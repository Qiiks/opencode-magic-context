export const DATABASE_REPAIR_COMMAND = "bunx @cortexkit/magic-context@latest doctor repair-db";

export function formatDatabaseRepairGuidance(dbPath: string): string {
    return `Database: ${dbPath}. Recovery: run \`${DATABASE_REPAIR_COMMAND}\`.`;
}
