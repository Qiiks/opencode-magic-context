import { createHash, randomUUID } from "node:crypto";
import { log } from "../../shared/logger";
import type { Database } from "../../shared/sqlite";
import { withPrivilegedWriter } from "../../shared/sqlite";

export const AUTHORITY_DOMAINS = ["memories", "notes"] as const;
export type AuthorityDomain = (typeof AUTHORITY_DOMAINS)[number];
export type AuthorityState = "TS" | "PREPARING" | "MODULE" | "DRAINING";

export interface AuthorityStatus {
    context_store_uuid: string;
    project: string;
    domain: AuthorityDomain;
    state: AuthorityState;
    generation: number;
    captured_upper_bound?: number | null;
    drain_cursor?: number;
    step_seed?: boolean;
    step_memories?: boolean;
    step_notes?: boolean;
    step_compartments?: boolean;
    step_reconcile?: boolean;
    step_verify?: boolean;
    step_flip?: boolean;
    coordinator_lease?: string | null;
    lease_expires_at?: number | null;
    /** Attempt-unique drain coordinator token minted at begin/takeover. */
    coordinator_token?: string | null;
    checksum_expected?: string | null;
    checksum_actual?: string | null;
    checksum_ok?: number | boolean | null;
}

export interface AuthorityModuleClient {
    authorityStatus(args: {
        context_store_uuid: string;
        project: string;
        projectRoot?: string;
        domain: AuthorityDomain;
    }): Promise<{ authority: AuthorityStatus | null }>;
    authorityPrepare(args: Record<string, unknown>): Promise<{ authority: AuthorityStatus }>;
    authorityDrain?(args: Record<string, unknown>): Promise<{ authority: AuthorityStatus }>;
    authoritySeed?(
        args: Record<string, unknown>,
    ): Promise<{ seeded: number; module_row_ids?: number[] }>;
    mirrorPull?(args: {
        domain: AuthorityDomain;
        cursor: number;
        limit: number;
        projectRoot?: string;
    }): Promise<{ page: ChangefeedPage }>;
}

export interface ModuleNoteEvaluationBridge {
    sync(): Promise<void>;
    evaluate(args: { contextNoteId: number; sessionId: string; verdict: boolean }): Promise<void>;
}

const moduleNoteEvaluationBridges = new Map<string, ModuleNoteEvaluationBridge>();

export function registerModuleNoteEvaluationBridge(
    projectPath: string,
    bridge: ModuleNoteEvaluationBridge,
): void {
    moduleNoteEvaluationBridges.set(projectPath, bridge);
}

export function getModuleNoteEvaluationBridge(
    projectPath: string,
): ModuleNoteEvaluationBridge | undefined {
    return moduleNoteEvaluationBridges.get(projectPath);
}

export interface ChangefeedRow {
    feed_seq: number;
    domain: AuthorityDomain;
    op: "insert" | "update" | "tombstone";
    module_row_id: number;
    full_row_snapshot: Record<string, unknown>;
    content_hash: string | null;
}

export interface ChangefeedPage {
    domain: AuthorityDomain;
    cursor: number;
    next_cursor: number;
    has_more: boolean;
    rows: ChangefeedRow[];
}

interface StoreMetaRow {
    value: string;
}

export function getContextStoreUuid(db: Database): string | null {
    const row = db.prepare("SELECT value FROM context_store_meta WHERE key = 'store_uuid'").get() as
        | StoreMetaRow
        | undefined;
    return typeof row?.value === "string" && row.value.length > 0 ? row.value : null;
}

/** Mint the store identity once. Restoring a database restores this value too,
 * which is what lets the module recognize a regressed marker. */
export function ensureContextStoreUuid(db: Database): string {
    const existing = getContextStoreUuid(db);
    if (existing) return existing;
    const minted = randomUUID();
    withPrivilegedWriter(db, () => {
        db.transaction(() => {
            db.prepare(
                "INSERT INTO context_store_meta(key, value) VALUES ('store_uuid', ?) ON CONFLICT(key) DO NOTHING",
            ).run(minted);
        }).immediate();
    });
    return getContextStoreUuid(db) ?? minted;
}

export interface AuthorityManagedMarker {
    project_path: string;
    context_store_uuid: string;
    marked_at: number;
}

export function getAuthorityManagedMarker(
    db: Database,
    projectPath: string,
): AuthorityManagedMarker | null {
    return (
        (db
            .prepare(
                "SELECT project_path, context_store_uuid, marked_at FROM authority_managed WHERE project_path = ?",
            )
            .get(projectPath) as AuthorityManagedMarker | undefined) ?? null
    );
}

export function listAuthorityManagedMarkers(db: Database): AuthorityManagedMarker[] {
    return db
        .prepare(
            "SELECT project_path, context_store_uuid, marked_at FROM authority_managed ORDER BY project_path",
        )
        .all() as AuthorityManagedMarker[];
}

export function installAuthorityManagedMarker(
    db: Database,
    projectPath: string,
    contextStoreUuid = ensureContextStoreUuid(db),
): void {
    withPrivilegedWriter(db, () => {
        db.prepare(
            "INSERT INTO authority_managed(project_path, context_store_uuid, marked_at) VALUES (?, ?, ?) ON CONFLICT(project_path) DO UPDATE SET context_store_uuid = excluded.context_store_uuid, marked_at = excluded.marked_at",
        ).run(projectPath, contextStoreUuid, Date.now());
    });
}

export function removeAuthorityManagedMarker(db: Database, projectPath: string): void {
    withPrivilegedWriter(db, () => {
        db.prepare("DELETE FROM authority_managed WHERE project_path = ?").run(projectPath);
    });
}

function setRepairPending(db: Database, projectPath: string): void {
    withPrivilegedWriter(db, () => {
        db.prepare(
            "INSERT INTO authority_repair_pending(project_path, started_at) VALUES (?, ?) ON CONFLICT(project_path) DO UPDATE SET started_at = excluded.started_at",
        ).run(projectPath, Date.now());
    });
}

function clearRepairPending(db: Database, projectPath: string): void {
    withPrivilegedWriter(db, () => {
        db.prepare("DELETE FROM authority_repair_pending WHERE project_path = ?").run(projectPath);
    });
}

/**
 * Repair a marker lost by restoring an older context.db snapshot. The write barrier
 * makes the repair atomic with the marker installation; callers keep application
 * writes closed until this function resolves.
 */
export async function reconcileAuthorityMarker(args: {
    db: Database;
    projectPath: string;
    module: AuthorityModuleClient;
}): Promise<{ status: "legacy" | "ok" | "repaired"; authority: AuthorityStatus | null }> {
    const contextStoreUuid = ensureContextStoreUuid(args.db);
    const marker = getAuthorityManagedMarker(args.db, args.projectPath);
    if (marker) {
        const statuses = await Promise.all(
            AUTHORITY_DOMAINS.map((domain) =>
                args.module.authorityStatus({
                    context_store_uuid: contextStoreUuid,
                    project: args.projectPath,
                    domain,
                }),
            ),
        );
        return {
            status: "ok",
            authority: statuses.find((result) => result.authority !== null)?.authority ?? null,
        };
    }

    // A missing marker is ambiguous until the module answers. Keep all writes closed
    // during that round-trip so a restored store cannot accept a write before repair.
    setRepairPending(args.db, args.projectPath);
    // Keep the durable pending marker if the module request fails: this host is expected
    // to reach the module, so an unknown result remains fail-closed until a later retry.
    const statuses: Array<{ authority: AuthorityStatus | null }> = await Promise.all(
        AUTHORITY_DOMAINS.map((domain) =>
            args.module.authorityStatus({
                context_store_uuid: contextStoreUuid,
                project: args.projectPath,
                domain,
            }),
        ),
    );
    const authority =
        statuses.find((result) => result.authority !== null && result.authority.state !== "TS")
            ?.authority ?? null;
    if (!authority) {
        clearRepairPending(args.db, args.projectPath);
        return { status: "legacy", authority: null };
    }

    // The module still owns this UUID, so a marker-less restore is regressed rather
    // than a new store. Hold the SQLite writer lock while reinstalling the fence.
    withPrivilegedWriter(args.db, () => {
        installAuthorityManagedMarker(args.db, args.projectPath, contextStoreUuid);
        args.db
            .prepare("DELETE FROM authority_repair_pending WHERE project_path = ?")
            .run(args.projectPath);
    });
    return { status: "repaired", authority };
}

export async function reconcileAuthorityProject(args: {
    db: Database;
    projectPath: string;
    module: AuthorityModuleClient;
}): Promise<void> {
    await reconcileAuthorityMarker(args);
    const contextStoreUuid = ensureContextStoreUuid(args.db);
    for (const domain of AUTHORITY_DOMAINS) {
        const status = await args.module.authorityStatus({
            context_store_uuid: contextStoreUuid,
            project: args.projectPath,
            domain,
        });
        if (status.authority?.state !== "MODULE") continue;
        const identity = args.db
            .prepare(
                "SELECT 1 FROM mirror_identity WHERE domain = ? AND module_project = ? LIMIT 1",
            )
            .get(domain, args.projectPath);
        if (identity) continue;
        if (!args.module.mirrorPull) {
            throw new Error(`authority reconciliation requires mirror.pull for ${domain}`);
        }
        withPrivilegedWriter(args.db, () => {
            args.db
                .transaction(() => {
                    args.db
                        .prepare(
                            "DELETE FROM mirror_identity WHERE domain = ? AND module_project = ?",
                        )
                        .run(domain, args.projectPath);
                    args.db
                        .prepare(
                            "DELETE FROM mirror_pending_references WHERE domain = ? AND module_project = ?",
                        )
                        .run(domain, args.projectPath);
                    if (domain === "notes") {
                        args.db
                            .prepare("DELETE FROM mirror_note_revisions WHERE module_project = ?")
                            .run(args.projectPath);
                    }
                    args.db
                        .prepare(
                            "INSERT INTO mirror_cursors(domain, cursor, updated_at) VALUES (?, 0, ?) ON CONFLICT(domain) DO UPDATE SET cursor = 0, updated_at = excluded.updated_at",
                        )
                        .run(domain, Date.now());
                })
                .immediate();
        });
        for (;;) {
            const cursor = getMirrorCursor(args.db, domain);
            const response = await args.module.mirrorPull({ domain, cursor, limit: 1000 });
            const next = applyMirrorPage({ db: args.db, page: response.page });
            if (!response.page.has_more || next === cursor) break;
        }
    }
}

export interface PrepareAuthorityArgs {
    db: Database;
    projectPath: string;
    domains?: readonly AuthorityDomain[];
    module: AuthorityModuleClient;
    seedPages: (domain: AuthorityDomain) => Promise<readonly Record<string, unknown>[]>;
    /** Test seam for alternate canonical encoders. Production uses the shared row digest. */
    checksum?: (domain: AuthorityDomain, rows: readonly Record<string, unknown>[]) => string;
}

function canonicalizeSeedValue(value: unknown): unknown {
    if (Array.isArray(value)) return value.map(canonicalizeSeedValue);
    if (value === null || typeof value !== "object") return value;
    const record = value as Record<string, unknown>;
    return Object.fromEntries(
        Object.keys(record)
            .sort()
            .map((key) => [key, canonicalizeSeedValue(record[key])]),
    );
}

export function checksumAuthoritySeedRows(rows: readonly Record<string, unknown>[]): string {
    const ordered = [...rows].sort((left, right) => {
        const leftId = seedSourceRowId(left) ?? Number.MAX_SAFE_INTEGER;
        const rightId = seedSourceRowId(right) ?? Number.MAX_SAFE_INTEGER;
        return leftId - rightId;
    });
    return createHash("sha256")
        .update(JSON.stringify(ordered.map(canonicalizeSeedValue)))
        .digest("hex");
}

function maxDomainRowId(db: Database, domain: AuthorityDomain, projectPath: string): number {
    const row = (
        domain === "memories"
            ? db
                  .prepare(
                      "SELECT COALESCE(MAX(rowid), 0) AS max_rowid FROM memories WHERE project_path = ?",
                  )
                  .get(projectPath)
            : db
                  .prepare(
                      `SELECT COALESCE(MAX(n.rowid), 0) AS max_rowid
                         FROM notes n
                        WHERE n.project_path = ?
                           OR (n.project_path IS NULL AND EXISTS (
                               SELECT 1 FROM session_projects sp
                                WHERE sp.session_id = n.session_id AND sp.project_path = ?
                           ))`,
                  )
                  .get(projectPath, projectPath)
    ) as { max_rowid?: number } | undefined;
    return typeof row?.max_rowid === "number" ? row.max_rowid : 0;
}

/** Read the transactionally maintained domain mutation epoch (0 when never bumped). */
export function readDomainMutationEpoch(
    db: Database,
    projectPath: string,
    domain: AuthorityDomain,
): number {
    const row = db
        .prepare("SELECT epoch FROM domain_mutation_epoch WHERE project_path = ? AND domain = ?")
        .get(projectPath, domain) as { epoch?: number } | undefined;
    return typeof row?.epoch === "number" ? row.epoch : 0;
}

/**
 * Bump the domain mutation epoch inside the current privileged write transaction.
 * Same-connection privileged UPDATEs do not advance PRAGMA data_version; this epoch
 * is the capture bound that detects those writes.
 */
export function bumpDomainMutationEpoch(
    db: Database,
    projectPath: string,
    domain: AuthorityDomain,
): void {
    db.prepare(
        `INSERT INTO domain_mutation_epoch(project_path, domain, epoch) VALUES (?, ?, 1)
         ON CONFLICT(project_path, domain) DO UPDATE SET epoch = epoch + 1`,
    ).run(projectPath, domain);
}

function installMarkerAndCaptureBounds(args: {
    db: Database;
    projectPath: string;
    contextStoreUuid: string;
    domains: readonly AuthorityDomain[];
}): void {
    args.db.exec("BEGIN IMMEDIATE");
    try {
        withPrivilegedWriter(args.db, () => {
            args.db
                .prepare(
                    "INSERT INTO authority_managed(project_path, context_store_uuid, marked_at) VALUES (?, ?, ?) ON CONFLICT(project_path) DO UPDATE SET context_store_uuid = excluded.context_store_uuid, marked_at = excluded.marked_at",
                )
                .run(args.projectPath, args.contextStoreUuid, Date.now());
            const capture = args.db.prepare(
                "INSERT INTO authority_capture_bounds(project_path, domain, max_rowid, data_version, mutation_epoch, captured_at) VALUES (?, ?, ?, 0, ?, ?) ON CONFLICT(project_path, domain) DO UPDATE SET max_rowid = excluded.max_rowid, data_version = excluded.data_version, mutation_epoch = excluded.mutation_epoch, captured_at = excluded.captured_at",
            );
            for (const domain of args.domains) {
                capture.run(
                    args.projectPath,
                    domain,
                    maxDomainRowId(args.db, domain, args.projectPath),
                    readDomainMutationEpoch(args.db, args.projectPath, domain),
                    Date.now(),
                );
            }
        });
        args.db.exec("COMMIT");
    } catch (error) {
        try {
            args.db.exec("ROLLBACK");
        } catch {
            // Preserve the capture failure.
        }
        throw error;
    }
}

function capturedBoundsUnchanged(
    db: Database,
    projectPath: string,
    domains: readonly AuthorityDomain[],
): boolean {
    db.exec("BEGIN IMMEDIATE");
    try {
        const read = db.prepare(
            "SELECT max_rowid, mutation_epoch FROM authority_capture_bounds WHERE project_path = ? AND domain = ?",
        );
        const unchanged = domains.every((domain) => {
            const captured = read.get(projectPath, domain) as
                | { max_rowid: number; mutation_epoch: number }
                | undefined;
            return (
                captured !== undefined &&
                captured.max_rowid === maxDomainRowId(db, domain, projectPath) &&
                captured.mutation_epoch === readDomainMutationEpoch(db, projectPath, domain)
            );
        });
        db.exec("COMMIT");
        return unchanged;
    } catch (error) {
        try {
            db.exec("ROLLBACK");
        } catch {
            // Preserve the verification failure.
        }
        throw error;
    }
}

export async function prepareAuthority(args: PrepareAuthorityArgs): Promise<AuthorityStatus[]> {
    const contextStoreUuid = ensureContextStoreUuid(args.db);
    const domains = args.domains ?? AUTHORITY_DOMAINS;
    if (!args.module.authoritySeed) {
        throw new Error("authority preparation requires the authority.seed module route");
    }

    installMarkerAndCaptureBounds({
        db: args.db,
        projectPath: args.projectPath,
        contextStoreUuid,
        domains,
    });

    const startedGenerations = new Map<AuthorityDomain, number>();
    const prepared: Array<{
        domain: AuthorityDomain;
        generation: number;
    }> = [];
    try {
        for (const domain of domains) {
            const started = await args.module.authorityPrepare({
                method: "authority.prepare",
                phase: "begin",
                context_store_uuid: contextStoreUuid,
                project: args.projectPath,
                domain,
            });
            startedGenerations.set(domain, started.authority.generation);
            const rows = await args.seedPages(domain);
            for (const page of chunkRowsForFrame(rows)) {
                const seedResponse = await args.module.authoritySeed({
                    method: "authority.seed",
                    context_store_uuid: contextStoreUuid,
                    project: args.projectPath,
                    domain,
                    rows: page,
                });
                for (const [index, moduleRowId] of (seedResponse.module_row_ids ?? []).entries()) {
                    const sourceRowId = seedSourceRowId(page[index]);
                    if (sourceRowId !== null) {
                        rememberIdentity(
                            args.db,
                            domain,
                            args.projectPath,
                            moduleRowId,
                            sourceRowId,
                        );
                    }
                }
            }
            const digest = args.checksum?.(domain, rows) ?? checksumAuthoritySeedRows(rows);
            const completed = await args.module.authorityPrepare({
                method: "authority.prepare",
                phase: "complete",
                context_store_uuid: contextStoreUuid,
                project: args.projectPath,
                domain,
                generation: started.authority.generation,
                checksum_expected: digest,
            });
            const authority = completed.authority;
            const checksumOk = authority.checksum_ok === true || authority.checksum_ok === 1;
            if (
                authority.state !== "PREPARING" ||
                !checksumOk ||
                authority.checksum_expected !== digest ||
                authority.checksum_actual !== digest
            ) {
                log(
                    `[magic-context] authority seed checksum mismatch for ${domain}; aborting module ownership`,
                );
                throw new Error(`authority seed verification failed for ${domain}`);
            }
            prepared.push({ domain, generation: started.authority.generation });
        }

        if (!capturedBoundsUnchanged(args.db, args.projectPath, domains)) {
            log(
                "[magic-context] authority capture bound drifted while writers were fenced; aborting module ownership",
            );
            throw new Error("authority capture bound changed while TypeScript writers were fenced");
        }

        const results: AuthorityStatus[] = [];
        for (const item of prepared) {
            const acknowledged = await args.module.authorityPrepare({
                method: "authority.prepare",
                phase: "ack",
                context_store_uuid: contextStoreUuid,
                project: args.projectPath,
                domain: item.domain,
                generation: item.generation,
            });
            if (acknowledged.authority.state !== "MODULE") {
                throw new Error(`authority acknowledgement failed for ${item.domain}`);
            }
            results.push(acknowledged.authority);
        }
        return results;
    } catch (error) {
        let moduleOwnsDomain = false;
        for (const [domain, generation] of startedGenerations) {
            try {
                const aborted = await args.module.authorityPrepare({
                    method: "authority.prepare",
                    phase: "abort",
                    context_store_uuid: contextStoreUuid,
                    project: args.projectPath,
                    domain,
                    generation,
                });
                moduleOwnsDomain ||= aborted.authority.state === "MODULE";
            } catch {
                moduleOwnsDomain = true;
            }
        }
        if (!moduleOwnsDomain) removeAuthorityManagedMarker(args.db, args.projectPath);
        throw error;
    }
}

export async function drainAuthority(args: {
    db: Database;
    projectPath: string;
    domain: AuthorityDomain;
    module: AuthorityModuleClient;
    checksum: string | (() => string);
    limit?: number;
}): Promise<AuthorityStatus> {
    if (!args.module.authorityDrain) {
        throw new Error("authority drain is unavailable on this module client");
    }
    if (!args.module.mirrorPull) {
        throw new Error("memory authority drain requires the mirror.pull module route");
    }
    const contextStoreUuid = ensureContextStoreUuid(args.db);
    const leaseStartedAt = Date.now();
    let status = (
        await args.module.authorityDrain({
            method: "authority.drain.begin",
            context_store_uuid: contextStoreUuid,
            project: args.projectPath,
            domain: args.domain,
            action: "begin",
            lease: `ts:${contextStoreUuid}`,
            lease_started_at: leaseStartedAt,
            lease_expires_at: leaseStartedAt + 60_000,
        })
    ).authority;
    const coordinatorToken = status.coordinator_token;
    if (typeof coordinatorToken !== "string" || coordinatorToken.length === 0) {
        throw new Error("authority drain begin omitted coordinator_token");
    }
    const upperBound = status.captured_upper_bound ?? status.drain_cursor ?? 0;
    while (getMirrorCursor(args.db, args.domain) < upperBound) {
        const cursor = getMirrorCursor(args.db, args.domain);
        const page = await args.module.mirrorPull({
            domain: args.domain,
            cursor,
            limit: Math.max(1, Math.min(args.limit ?? 100, 1000)),
        });
        applyMirrorPage({ db: args.db, page: page.page });
        const next = getMirrorCursor(args.db, args.domain);
        if (next === cursor) break;
    }
    for (const step of [
        "seed",
        "memories",
        "notes",
        "compartments",
        "reconcile",
        "verify",
    ] as const) {
        status = (
            await args.module.authorityDrain({
                method: `authority.drain_${step}`,
                context_store_uuid: contextStoreUuid,
                project: args.projectPath,
                domain: args.domain,
                action: step,
                generation: status.generation,
                cursor: getMirrorCursor(args.db, args.domain),
                coordinator_token: coordinatorToken,
                now_ms: Date.now(),
            })
        ).authority;
    }
    const drainChecksum = typeof args.checksum === "function" ? args.checksum() : args.checksum;
    const finished = (
        await args.module.authorityDrain({
            method: "authority.drain.finish",
            context_store_uuid: contextStoreUuid,
            project: args.projectPath,
            domain: args.domain,
            action: "finish",
            generation: status.generation,
            checksum_expected: drainChecksum,
            checksum_actual: drainChecksum,
            verified: true,
            coordinator_token: coordinatorToken,
            now_ms: Date.now(),
        })
    ).authority;
    if (finished.state !== "TS") {
        throw new Error("memory authority drain did not reactivate TypeScript ownership");
    }
    // A project marker fences both authority domains. Remove it only after neither
    // domain remains module-owned; a one-domain drain must not reopen the other domain.
    const remaining = await Promise.all(
        AUTHORITY_DOMAINS.map((domain) =>
            args.module.authorityStatus({
                context_store_uuid: contextStoreUuid,
                project: args.projectPath,
                domain,
            }),
        ),
    );
    if (remaining.every((result) => !result.authority || result.authority.state === "TS")) {
        removeAuthorityManagedMarker(args.db, args.projectPath);
    }
    return finished;
}

function seedSourceRowId(row: Record<string, unknown>): number | null {
    const direct = row.source_row_id;
    if (typeof direct === "number" && Number.isInteger(direct)) return direct;
    const snapshot = row.snapshot;
    if (snapshot && typeof snapshot === "object" && !Array.isArray(snapshot)) {
        const sourceId = (snapshot as Record<string, unknown>).id;
        return typeof sourceId === "number" && Number.isInteger(sourceId) ? sourceId : null;
    }
    const id = row.id;
    return typeof id === "number" && Number.isInteger(id) ? id : null;
}

const MAX_AUTHORITY_SEED_FRAME_BYTES = 900 * 1024;

function chunkRowsForFrame<T>(rows: readonly T[]): T[][] {
    const chunks: T[][] = [];
    let current: T[] = [];
    let currentBytes = 2;
    for (const row of rows) {
        const rowBytes = new TextEncoder().encode(JSON.stringify(row)).byteLength + 1;
        if (current.length > 0 && currentBytes + rowBytes > MAX_AUTHORITY_SEED_FRAME_BYTES) {
            chunks.push(current);
            current = [];
            currentBytes = 2;
        }
        current.push(row);
        currentBytes += rowBytes;
    }
    if (current.length > 0) chunks.push(current);
    return chunks;
}

export function getMirrorCursor(db: Database, domain: AuthorityDomain): number {
    const row = db.prepare("SELECT cursor FROM mirror_cursors WHERE domain = ?").get(domain) as
        | { cursor?: number }
        | undefined;
    return typeof row?.cursor === "number" ? row.cursor : 0;
}

function rowNumber(row: Record<string, unknown>, key: string, fallback = 0): number {
    const value = row[key];
    return typeof value === "number" && Number.isFinite(value) ? value : fallback;
}

function rowString(row: Record<string, unknown>, key: string, fallback = ""): string {
    const value = row[key];
    return typeof value === "string" ? value : fallback;
}

function rowNullableString(row: Record<string, unknown>, key: string): string | null {
    const value = row[key];
    return typeof value === "string" ? value : null;
}

function mirrorIdentity(
    db: Database,
    domain: AuthorityDomain,
    moduleProject: string,
    moduleRowId: number,
): { context_row_id: number } | null {
    return (
        (db
            .prepare(
                "SELECT context_row_id FROM mirror_identity WHERE domain = ? AND module_project = ? AND module_row_id = ?",
            )
            .get(domain, moduleProject, moduleRowId) as { context_row_id: number } | undefined) ??
        null
    );
}

function rememberIdentity(
    db: Database,
    domain: AuthorityDomain,
    moduleProject: string,
    moduleRowId: number,
    contextRowId: number,
): void {
    const existing = mirrorIdentity(db, domain, moduleProject, moduleRowId);
    if (existing) return;
    // A context row has one canonical module identity. A duplicate feed row may
    // still update that row, but it must not claim a second identity for it.
    db.prepare(
        "INSERT OR IGNORE INTO mirror_identity(domain, module_project, module_row_id, context_row_id) VALUES (?, ?, ?, ?)",
    ).run(domain, moduleProject, moduleRowId, contextRowId);
}

function contextMemoryId(
    db: Database,
    domain: AuthorityDomain,
    moduleProject: string,
    row: Record<string, unknown>,
    moduleRowId: number,
): number {
    const mapped = mirrorIdentity(db, domain, moduleProject, moduleRowId);
    if (mapped) return mapped.context_row_id;
    const sourceUuid = rowNullableString(row, "context_store_uuid");
    const sourceId = rowNumber(row, "context_row_id", -1);
    if (sourceUuid && sourceId >= 0) {
        const existing = db
            .prepare("SELECT id FROM memories WHERE id = ? AND project_path = ?")
            .get(sourceId, moduleProject) as { id?: number } | undefined;
        if (existing?.id !== undefined) {
            rememberIdentity(db, domain, moduleProject, moduleRowId, existing.id);
            return existing.id;
        }
    }
    // A legacy facade row may have no source identity even though the context
    // database already owns the same fact. Adopt an unambiguous content match
    // instead of creating a second context row during mirror-back.
    const normalizedHash = rowString(row, "normalized_hash");
    const category = rowString(row, "category", "CONSTRAINTS");
    if (normalizedHash) {
        const candidates = db
            .prepare(
                "SELECT id FROM memories WHERE category = ? AND normalized_hash = ? ORDER BY id",
            )
            .all(category, normalizedHash) as Array<{ id?: number }>;
        if (candidates.length === 1 && candidates[0]?.id !== undefined) {
            rememberIdentity(db, domain, moduleProject, moduleRowId, candidates[0].id);
            return candidates[0].id;
        }
    }
    const result = db
        .prepare(
            "INSERT INTO memories (project_path, category, content, normalized_hash, first_seen_at, created_at, updated_at, last_seen_at) VALUES (?, ?, '', '', 0, 0, 0, 0)",
        )
        .run(moduleProject, rowString(row, "category", "CONSTRAINTS"));
    const contextId = Number(result.lastInsertRowid);
    rememberIdentity(db, domain, moduleProject, moduleRowId, contextId);
    return contextId;
}

function applyMemoryRow(db: Database, feed: ChangefeedRow): void {
    const row = feed.full_row_snapshot;
    const moduleProject = rowString(row, "project_path");
    if (!moduleProject) throw new Error("memory feed snapshot has no project_path");
    if (feed.op === "tombstone") {
        db.prepare(
            "DELETE FROM mirror_live_memory_rows WHERE module_project = ? AND module_row_id = ?",
        ).run(moduleProject, feed.module_row_id);
        // Drop pending refs even when the tombstoned module row was never mapped
        // locally; otherwise a forward reference to a never-seen target leaks forever.
        db.prepare(
            "DELETE FROM mirror_pending_references WHERE domain = ? AND module_project = ? AND (module_row_id = ? OR target_module_row_id = ?)",
        ).run(feed.domain, moduleProject, feed.module_row_id, feed.module_row_id);
        const mapped = mirrorIdentity(db, feed.domain, moduleProject, feed.module_row_id);
        if (!mapped) return;
        db.prepare(
            "DELETE FROM mirror_identity WHERE domain = ? AND module_project = ? AND module_row_id = ?",
        ).run(feed.domain, moduleProject, feed.module_row_id);
        const shared = db
            .prepare(
                "SELECT 1 FROM mirror_identity WHERE domain = ? AND context_row_id = ? LIMIT 1",
            )
            .get(feed.domain, mapped.context_row_id);
        // A cleanup tombstone for one legacy twin must not delete the context row
        // still owned by another module identity for the same source memory.
        if (shared) return;
        const contextRow = db
            .prepare("SELECT project_path, category, normalized_hash FROM memories WHERE id = ?")
            .get(mapped.context_row_id) as
            | { project_path?: string; category?: string; normalized_hash?: string }
            | undefined;
        if (contextRow?.project_path && contextRow.category && contextRow.normalized_hash) {
            const liveMatches = db
                .prepare(
                    `SELECT module_project, module_row_id FROM mirror_live_memory_rows
                     WHERE module_project = ? AND category = ? AND normalized_hash = ?
                     ORDER BY module_row_id LIMIT 2`,
                )
                .all(
                    contextRow.project_path,
                    contextRow.category,
                    contextRow.normalized_hash,
                ) as Array<{ module_project: string; module_row_id: number }>;
            if (liveMatches.length === 1 && liveMatches[0]) {
                // Feed inserts can race for the mirror's one identity slot. Rechecking live
                // content after removing the tombstoned owner preserves the canonical row
                // without weakening the one-context-row/one-module-row invariant.
                rememberIdentity(
                    db,
                    feed.domain,
                    liveMatches[0].module_project,
                    liveMatches[0].module_row_id,
                    mapped.context_row_id,
                );
                return;
            }
        }
        db.prepare("DELETE FROM memory_embeddings WHERE memory_id = ?").run(mapped.context_row_id);
        db.prepare("DELETE FROM memories WHERE id = ?").run(mapped.context_row_id);
        return;
    }
    db.prepare(
        `INSERT INTO mirror_live_memory_rows(module_project, module_row_id, category, normalized_hash)
         VALUES (?, ?, ?, ?)
         ON CONFLICT(module_project, module_row_id) DO UPDATE SET
             category = excluded.category,
             normalized_hash = excluded.normalized_hash`,
    ).run(
        moduleProject,
        feed.module_row_id,
        rowString(row, "category", "CONSTRAINTS"),
        rowString(row, "normalized_hash"),
    );
    const contextId = contextMemoryId(db, feed.domain, moduleProject, row, feed.module_row_id);
    const previous = db
        .prepare("SELECT normalized_hash FROM memories WHERE id = ?")
        .get(contextId) as { normalized_hash?: string } | undefined;
    db.prepare(
        `UPDATE memories SET project_path = ?, category = ?, content = ?, normalized_hash = ?,
         importance = ?, scope = ?, shareable = ?, source_session_id = ?, source_type = ?,
         seen_count = ?, retrieval_count = ?, first_seen_at = ?, created_at = ?, updated_at = ?,
         last_seen_at = ?, last_retrieved_at = ?, status = ?, expires_at = ?,
         verification_status = ?, verified_at = ?, classified_at = ?, superseded_by_memory_id = ?,
         merged_from = ?, metadata_json = ? WHERE id = ?`,
    ).run(
        moduleProject,
        rowString(row, "category", "CONSTRAINTS"),
        rowString(row, "content"),
        rowString(row, "normalized_hash"),
        row.importance ?? null,
        rowString(row, "scope", "project"),
        rowNumber(row, "shareable"),
        rowNullableString(row, "source_session_id"),
        rowNullableString(row, "source_type"),
        rowNumber(row, "seen_count", 1),
        rowNumber(row, "retrieval_count"),
        rowNumber(row, "first_seen_at"),
        rowNumber(row, "created_at"),
        rowNumber(row, "updated_at"),
        rowNumber(row, "last_seen_at"),
        typeof row.last_retrieved_at === "number" ? row.last_retrieved_at : null,
        rowString(row, "status", "active"),
        typeof row.expires_at === "number" ? row.expires_at : null,
        rowString(row, "verification_status", "unverified"),
        typeof row.verified_at === "number" ? row.verified_at : null,
        typeof row.classified_at === "number" ? row.classified_at : null,
        null,
        rowNullableString(row, "merged_from"),
        rowNullableString(row, "metadata_json"),
        contextId,
    );
    if (typeof row.superseded_by_memory_id === "number") {
        const translated = mirrorIdentity(
            db,
            "memories",
            moduleProject,
            row.superseded_by_memory_id,
        );
        if (translated) {
            db.prepare("UPDATE memories SET superseded_by_memory_id = ? WHERE id = ?").run(
                translated.context_row_id,
                contextId,
            );
            db.prepare(
                "DELETE FROM mirror_pending_references WHERE domain = 'memories' AND module_project = ? AND module_row_id = ?",
            ).run(moduleProject, feed.module_row_id);
        } else {
            db.prepare(
                "INSERT INTO mirror_pending_references(domain, module_project, module_row_id, target_module_row_id) VALUES ('memories', ?, ?, ?) ON CONFLICT(domain, module_project, module_row_id) DO UPDATE SET target_module_row_id = excluded.target_module_row_id",
            ).run(moduleProject, feed.module_row_id, row.superseded_by_memory_id);
        }
    } else {
        db.prepare(
            "DELETE FROM mirror_pending_references WHERE domain = 'memories' AND module_project = ? AND module_row_id = ?",
        ).run(moduleProject, feed.module_row_id);
    }
    if (previous?.normalized_hash !== feed.content_hash) {
        db.prepare("DELETE FROM memory_embeddings WHERE memory_id = ?").run(contextId);
    }
}

function contextNoteId(db: Database, feed: ChangefeedRow, moduleProject: string): number {
    const mapped = mirrorIdentity(db, feed.domain, moduleProject, feed.module_row_id);
    if (mapped) return mapped.context_row_id;
    const row = feed.full_row_snapshot;
    const sourceId = rowNumber(row, "context_row_id", -1);
    const sourceUuid = rowNullableString(row, "context_store_uuid");
    if (sourceUuid && sourceId >= 0) {
        const existing = db
            .prepare("SELECT id FROM notes WHERE id = ? AND type = 'smart' AND project_path = ?")
            .get(sourceId, moduleProject) as { id?: number } | undefined;
        if (existing?.id !== undefined) {
            rememberIdentity(db, feed.domain, moduleProject, feed.module_row_id, existing.id);
            return existing.id;
        }
    }
    const result = db
        .prepare(
            "INSERT INTO notes (type, status, content, project_path, session_id, created_at, updated_at) VALUES ('smart', 'active', '', ?, ?, 0, 0)",
        )
        .run(moduleProject, rowNullableString(row, "session_id"));
    const contextId = Number(result.lastInsertRowid);
    rememberIdentity(db, feed.domain, moduleProject, feed.module_row_id, contextId);
    return contextId;
}

function translateMemoryReferences(db: Database): void {
    const pending = db
        .prepare(
            `SELECT pending.module_project, pending.module_row_id, pending.target_module_row_id,
                    source.context_row_id AS source_context_id,
                    target.context_row_id AS target_context_id
               FROM mirror_pending_references pending
               JOIN mirror_identity source
                 ON source.domain = pending.domain
                AND source.module_project = pending.module_project
                AND source.module_row_id = pending.module_row_id
               JOIN mirror_identity target
                 ON target.domain = pending.domain
                AND target.module_project = pending.module_project
                AND target.module_row_id = pending.target_module_row_id
              WHERE pending.domain = 'memories'`,
        )
        .all() as Array<{
        module_project: string;
        module_row_id: number;
        target_module_row_id: number;
        source_context_id: number;
        target_context_id: number;
    }>;
    const update = db.prepare("UPDATE memories SET superseded_by_memory_id = ? WHERE id = ?");
    const clear = db.prepare(
        "DELETE FROM mirror_pending_references WHERE domain = 'memories' AND module_project = ? AND module_row_id = ?",
    );
    for (const reference of pending) {
        update.run(reference.target_context_id, reference.source_context_id);
        clear.run(reference.module_project, reference.module_row_id);
    }
}

function applyNoteRow(db: Database, feed: ChangefeedRow): void {
    const row = feed.full_row_snapshot;
    const moduleProject = rowString(row, "project_path");
    if (!moduleProject) throw new Error("note feed snapshot has no project_path");
    if (feed.op === "tombstone") {
        const mapped = mirrorIdentity(db, feed.domain, moduleProject, feed.module_row_id);
        if (!mapped) return;
        db.prepare("DELETE FROM notes WHERE id = ?").run(mapped.context_row_id);
        db.prepare(
            "DELETE FROM mirror_identity WHERE domain = ? AND module_project = ? AND module_row_id = ?",
        ).run(feed.domain, moduleProject, feed.module_row_id);
        db.prepare(
            "DELETE FROM mirror_note_revisions WHERE module_project = ? AND module_row_id = ?",
        ).run(moduleProject, feed.module_row_id);
        return;
    }
    const contextId = contextNoteId(db, feed, moduleProject);
    // Delivery-only module states are collapsed to the TS vocabulary. The ledger remains
    // authoritative for at-least-once delivery; context.db must not invent a new status.
    const moduleStatus = rowString(row, "status", "active");
    const contextStatus =
        moduleStatus === "surfaced" || moduleStatus === "surfacing" ? "ready" : moduleStatus;
    db.prepare(
        `UPDATE notes SET type = ?, status = ?, project_path = ?, session_id = ?, content = ?,
         surface_condition = ?, ready_at = ?, ready_reason = ?, manifest_json = ?, compiled_check = ?,
         check_hash = ?, check_cron = ?, check_failure_count = ?, check_network_failure_count = ?,
         check_quarantined_until = ?, check_next_due_at = ?, check_compiled_at = ?, check_false_since_at = ?,
         check_last_liveness_at = ?, last_checked_at = ?, check_status = ?, check_version = ?,
         policy_version = ?, anchor_block_id = ?, anchor_ordinal = ?, created_at = ?, updated_at = ? WHERE id = ?`,
    ).run(
        rowString(row, "type", "smart"),
        contextStatus,
        moduleProject,
        rowNullableString(row, "session_id"),
        rowString(row, "content"),
        rowNullableString(row, "surface_condition"),
        typeof row.ready_at === "number" ? row.ready_at : null,
        rowNullableString(row, "ready_reason"),
        rowNullableString(row, "manifest_json"),
        rowNullableString(row, "compiled_check"),
        rowNullableString(row, "check_hash"),
        rowNullableString(row, "check_cron"),
        rowNumber(row, "check_failure_count"),
        rowNumber(row, "check_network_failure_count"),
        typeof row.check_quarantined_until === "number" ? row.check_quarantined_until : null,
        typeof row.check_next_due_at === "number" ? row.check_next_due_at : null,
        typeof row.check_compiled_at === "number" ? row.check_compiled_at : null,
        typeof row.check_false_since_at === "number" ? row.check_false_since_at : null,
        typeof row.check_last_liveness_at === "number" ? row.check_last_liveness_at : null,
        typeof row.last_checked_at === "number" ? row.last_checked_at : null,
        rowString(row, "check_status", "uncompiled"),
        rowNumber(row, "check_version"),
        rowNumber(row, "policy_version", 1),
        rowNullableString(row, "anchor_block_id"),
        typeof row.anchor_ordinal === "number" ? row.anchor_ordinal : null,
        rowNumber(row, "created_at_ms"),
        rowNumber(row, "updated_at_ms"),
        contextId,
    );
    db.prepare(
        "INSERT INTO mirror_note_revisions(module_project, module_row_id, context_row_id, status_version) VALUES (?, ?, ?, ?) ON CONFLICT(module_project, module_row_id) DO UPDATE SET context_row_id = excluded.context_row_id, status_version = excluded.status_version",
    ).run(moduleProject, feed.module_row_id, contextId, rowNumber(row, "status_version"));
}

export function applyMirrorPage(args: { db: Database; page: ChangefeedPage }): number {
    const { db, page } = args;
    if (!AUTHORITY_DOMAINS.includes(page.domain)) throw new Error("unknown mirror domain");
    const durableCursor = getMirrorCursor(db, page.domain);
    if (page.cursor !== durableCursor) {
        throw new Error(
            `mirror cursor mismatch for ${page.domain}: expected ${durableCursor}, got ${page.cursor}`,
        );
    }
    let nextCursor = durableCursor;
    withPrivilegedWriter(db, () => {
        db.transaction(() => {
            const touchedProjects = new Set<string>();
            for (const feed of page.rows) {
                if (feed.domain !== page.domain || feed.feed_seq <= nextCursor) continue;
                const projectPath = rowString(feed.full_row_snapshot, "project_path");
                if (projectPath) touchedProjects.add(projectPath);
                if (feed.domain === "memories") applyMemoryRow(db, feed);
                else applyNoteRow(db, feed);
                nextCursor = feed.feed_seq;
            }
            translateMemoryReferences(db);
            for (const projectPath of touchedProjects) {
                bumpDomainMutationEpoch(db, projectPath, page.domain);
            }
            if (page.next_cursor < nextCursor) {
                throw new Error("mirror page moved its cursor backwards");
            }
            nextCursor = Math.max(nextCursor, page.next_cursor);
            db.prepare(
                "INSERT INTO mirror_cursors(domain, cursor, updated_at) VALUES (?, ?, ?) ON CONFLICT(domain) DO UPDATE SET cursor = excluded.cursor, updated_at = excluded.updated_at",
            ).run(page.domain, nextCursor, Date.now());
        }).immediate();
    });
    return nextCursor;
}

export async function pullAndApplyMirrorPage(args: {
    db: Database;
    module: AuthorityModuleClient;
    domain: AuthorityDomain;
    limit?: number;
}): Promise<number> {
    if (!args.module.mirrorPull) {
        throw new Error("memory mirror consumer requires the mirror.pull module route");
    }
    const cursor = getMirrorCursor(args.db, args.domain);
    const response = await args.module.mirrorPull({
        domain: args.domain,
        cursor,
        limit: Math.max(1, Math.min(args.limit ?? 100, 1000)),
    });
    return applyMirrorPage({ db: args.db, page: response.page });
}

const mirrorFlights = new WeakMap<object, Promise<number>>();

/**
 * The rust transform pass is the mirror cadence. Coalesce overlapping passes so a
 * slower pull can never race a second cursor application on the same connection.
 */
export function pullMemoryMirrorOnce(args: {
    db: Database;
    module: AuthorityModuleClient;
    limit?: number;
}): Promise<number> {
    const existing = mirrorFlights.get(args.module);
    if (existing) return existing;
    const flight = pullAndApplyMirrorPage({
        db: args.db,
        module: args.module,
        domain: "memories",
        limit: args.limit,
    }).finally(() => mirrorFlights.delete(args.module));
    mirrorFlights.set(args.module, flight);
    return flight;
}
