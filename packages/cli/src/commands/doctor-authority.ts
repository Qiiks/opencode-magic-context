import {
    checksumAuthoritySeedRows,
    drainAuthority,
    ensureContextStoreUuid,
    getAuthorityManagedMarker,
    listAuthorityManagedMarkers,
    type AuthorityModuleClient,
} from "@magic-context/core/features/magic-context/context-authority";
import { resolveProjectIdentity } from "@magic-context/core/features/magic-context/memory/project-identity";
import { bumpProjectMemoryEpoch } from "@magic-context/core/features/magic-context/storage-project-state";
import { SubcModuleTransport } from "@magic-context/core/hooks/magic-context/module-transport";
import type { Database } from "@magic-context/core/shared/sqlite";

import { openExistingContextDatabaseForMutation } from "../lib/database-access";

const DOMAINS = ["memories", "notes"] as const;

function authorityClient(transport: SubcModuleTransport, projectRoot: string): AuthorityModuleClient {
    return {
        authorityStatus: (request) => transport.authorityStatus({ ...request, projectRoot }),
        authorityPrepare: (request) => transport.authorityPrepare({ ...request, projectRoot }),
        authorityDrain: (request) => transport.authorityDrain({ ...request, projectRoot }),
        mirrorPull: (request) => transport.mirrorPull({ ...request, projectRoot }),
    };
}

function checksumFor(db: Database, projectPath: string, domain: (typeof DOMAINS)[number]): string {
    const table = domain === "memories" ? "memories" : "notes";
    const rows = db
        .prepare(`SELECT * FROM ${table} WHERE project_path = ? ORDER BY id ASC`)
        .all(projectPath)
        .filter((row): row is Record<string, unknown> => row !== null && typeof row === "object");
    return checksumAuthoritySeedRows(rows);
}

export async function reportAuthorityMarkers(args: {
    db: Database;
    info(message: string): void;
    warn(message: string): void;
}): Promise<void> {
    const markers = listAuthorityManagedMarkers(args.db);
    args.info("Authority:");
    if (markers.length === 0) {
        args.info("  no authority_managed markers");
        return;
    }

    let currentIdentity: string | undefined;
    try {
        currentIdentity = resolveProjectIdentity(process.cwd());
    } catch {
        // A doctor run must still report the durable fences when cwd identity fails.
    }
    const transport = new SubcModuleTransport();
    for (const marker of markers) {
        if (marker.project_path !== currentIdentity) {
            args.warn(
                `  ${marker.project_path}: module state unavailable outside its project root — writes fenced; run with rust mode or restore subc connectivity`,
            );
            continue;
        }
        try {
            const module = authorityClient(transport, process.cwd());
            const statuses = await Promise.all(
                DOMAINS.map((domain) =>
                    module.authorityStatus({
                        context_store_uuid: ensureContextStoreUuid(args.db),
                        project: marker.project_path,
                        domain,
                    }),
                ),
            );
            args.info(
                `  ${marker.project_path}: ${statuses
                    .map((status, index) => `${DOMAINS[index]}=${status.authority?.state ?? "TS"}`)
                    .join(", ")}`,
            );
        } catch {
            args.warn(
                `  ${marker.project_path}: module unreachable — writes fenced; run with rust mode or restore subc connectivity`,
            );
        }
    }
}

export async function runDoctorDrainAuthority(projectRoot: string, dbPath: string): Promise<number> {
    const db = openExistingContextDatabaseForMutation(dbPath);
    if (!db) {
        console.error("No Magic Context database found.");
        return 1;
    }
    try {
        const projectPath = resolveProjectIdentity(projectRoot);
        if (!getAuthorityManagedMarker(db, projectPath)) {
            console.log(`No authority_managed marker exists for ${projectPath}.`);
            return 0;
        }
        const module = authorityClient(new SubcModuleTransport(), projectRoot);
        let drainedAny = false;
        for (const domain of DOMAINS) {
            const status = await module.authorityStatus({
                context_store_uuid: ensureContextStoreUuid(db),
                project: projectPath,
                domain,
            });
            if (!status.authority || status.authority.state === "TS") continue;
            if (status.authority.state !== "MODULE" && status.authority.state !== "DRAINING") {
                console.error(`Authority ${domain} is ${status.authority.state}; retry after it settles.`);
                return 1;
            }
            let result: Awaited<ReturnType<typeof drainAuthority>> | undefined;
            for (let attempt = 0; attempt < 2; attempt += 1) {
                result = await drainAuthority({
                    db,
                    projectPath,
                    domain,
                    module,
                    checksum: () => checksumFor(db, projectPath, domain),
                });
                if (!("code" in result)) break;
            }
            if (!result || "code" in result) {
                console.error("Authority drain is contended and remains retryable; try again shortly.");
                return 1;
            }
            drainedAny = true;
        }
        if (drainedAny && !getAuthorityManagedMarker(db, projectPath)) {
            bumpProjectMemoryEpoch(db, projectPath);
            console.log(`Authority drained back to TypeScript for ${projectPath}.`);
            return 0;
        }
        console.error("Module did not confirm a complete authority drain; writes remain fenced.");
        return 1;
    } catch (error) {
        console.error(
            `Module unreachable — writes fenced; run with rust mode or restore subc connectivity: ${error instanceof Error ? error.message : String(error)}`,
        );
        return 1;
    } finally {
        db.close();
    }
}
