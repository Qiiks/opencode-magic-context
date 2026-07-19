import { randomUUID } from "node:crypto";
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
    checksum_expected?: string | null;
    checksum_actual?: string | null;
    checksum_ok?: number | boolean | null;
}

export interface AuthorityModuleClient {
    authorityStatus(args: {
        context_store_uuid: string;
        project: string;
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
    }): Promise<{ page: ChangefeedPage }>;
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
    const authority = statuses.find((result) => result.authority !== null)?.authority ?? null;
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

export interface PrepareAuthorityArgs {
    db: Database;
    projectPath: string;
    domains?: readonly AuthorityDomain[];
    module: AuthorityModuleClient;
    seedPages: (domain: AuthorityDomain) => Promise<readonly Record<string, unknown>[]>;
    checksum: (domain: AuthorityDomain) => string;
    /** Optional per-row verification hook used to fail closed before the flip. */
    verifySeed?: (
        domain: AuthorityDomain,
        rows: readonly Record<string, unknown>[],
    ) => boolean | Promise<boolean>;
}

/**
 * Run the TS-side PREPARING protocol. BEGIN IMMEDIATE intentionally remains open
 * while module round-trips run: SQLite readers do not conflict with this lock, while
 * every competing writer must commit or roll back before the seed bound is captured.
 */
export async function prepareAuthority(args: PrepareAuthorityArgs): Promise<AuthorityStatus[]> {
    const contextStoreUuid = ensureContextStoreUuid(args.db);
    const domains = args.domains ?? AUTHORITY_DOMAINS;
    const results: AuthorityStatus[] = [];
    if (!args.module.authoritySeed) {
        throw new Error("memory authority preparation requires the authority.seed module route");
    }
    let removeMarkerAfterAbort = false;
    args.db.exec("BEGIN IMMEDIATE");
    try {
        withPrivilegedWriter(args.db, () => {
            args.db
                .prepare(
                    "INSERT INTO authority_managed(project_path, context_store_uuid, marked_at) VALUES (?, ?, ?) ON CONFLICT(project_path) DO UPDATE SET context_store_uuid = excluded.context_store_uuid, marked_at = excluded.marked_at",
                )
                .run(args.projectPath, contextStoreUuid, Date.now());
        });
        for (const domain of domains) {
            const started = await args.module.authorityPrepare({
                method: "authority.prepare",
                phase: "begin",
                context_store_uuid: contextStoreUuid,
                project: args.projectPath,
                domain,
            });
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
            const digest = args.checksum(domain);
            const verified = (await args.verifySeed?.(domain, rows)) ?? true;
            if (!verified) {
                removeMarkerAfterAbort = true;
                await args.module.authorityPrepare({
                    method: "authority.prepare",
                    phase: "abort",
                    context_store_uuid: contextStoreUuid,
                    project: args.projectPath,
                    domain,
                    generation: started.authority.generation,
                });
                throw new Error(`memory authority seed verification failed for ${domain}`);
            }
            const completed = await args.module.authorityPrepare({
                method: "authority.prepare",
                phase: "complete",
                context_store_uuid: contextStoreUuid,
                project: args.projectPath,
                domain,
                generation: started.authority.generation,
                checksum_expected: digest,
                checksum_actual: digest,
                verified,
            });
            const authority = completed.authority;
            const moduleVerified =
                authority.checksum_ok === undefined ||
                authority.checksum_ok === null ||
                authority.checksum_ok === true ||
                authority.checksum_ok === 1;
            const checksumsMatch =
                authority.checksum_expected === undefined ||
                authority.checksum_expected === null ||
                authority.checksum_actual === undefined ||
                authority.checksum_actual === null ||
                authority.checksum_expected === authority.checksum_actual;
            if (authority.state !== "MODULE" || !moduleVerified || !checksumsMatch) {
                removeMarkerAfterAbort = true;
                if (authority.state === "PREPARING") {
                    try {
                        await args.module.authorityPrepare({
                            method: "authority.prepare",
                            phase: "abort",
                            context_store_uuid: contextStoreUuid,
                            project: args.projectPath,
                            domain,
                            generation: started.authority.generation,
                        });
                    } catch {
                        // Preserve the verification failure; the durable PREPARING row
                        // remains visible for a later repair attempt if abort is unavailable.
                    }
                }
                throw new Error(`memory authority seed verification failed for ${domain}`);
            }
            results.push(authority);
        }
        args.db.exec("COMMIT");
        return results;
    } catch (error) {
        try {
            args.db.exec("ROLLBACK");
        } catch {
            // Preserve the original preparation error.
        }
        if (removeMarkerAfterAbort) removeAuthorityManagedMarker(args.db, args.projectPath);
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
    let status = (
        await args.module.authorityDrain({
            method: "authority.drain.begin",
            context_store_uuid: contextStoreUuid,
            project: args.projectPath,
            domain: args.domain,
            action: "begin",
            lease: `ts:${contextStoreUuid}`,
            lease_expires_at: Date.now() + 60_000,
        })
    ).authority;
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
        })
    ).authority;
    if (finished.state !== "TS") {
        throw new Error("memory authority drain did not reactivate TypeScript ownership");
    }
    // Remove the authority-managed marker under an exclusive context.db write lock
    // immediately after the module flip, so TS writers cannot resume before the fence is gone.
    removeAuthorityManagedMarker(args.db, args.projectPath);
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
    db.prepare(
        "INSERT INTO mirror_identity(domain, module_project, module_row_id, context_row_id) VALUES (?, ?, ?, ?) ON CONFLICT(domain, module_project, module_row_id) DO UPDATE SET context_row_id = excluded.context_row_id",
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
        const mapped = mirrorIdentity(db, feed.domain, moduleProject, feed.module_row_id);
        if (!mapped) return;
        db.prepare("DELETE FROM memory_embeddings WHERE memory_id = ?").run(mapped.context_row_id);
        db.prepare("DELETE FROM memories WHERE id = ?").run(mapped.context_row_id);
        db.prepare(
            "DELETE FROM mirror_identity WHERE domain = ? AND module_project = ? AND module_row_id = ?",
        ).run(feed.domain, moduleProject, feed.module_row_id);
        return;
    }
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
        typeof row.superseded_by_memory_id === "number"
            ? (mirrorIdentity(db, "memories", moduleProject, row.superseded_by_memory_id)
                  ?.context_row_id ?? null)
            : null,
        rowNullableString(row, "merged_from"),
        rowNullableString(row, "metadata_json"),
        contextId,
    );
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
            "INSERT INTO notes (type, status, content, project_path, created_at, updated_at) VALUES ('smart', 'active', '', ?, 0, 0)",
        )
        .run(moduleProject);
    const contextId = Number(result.lastInsertRowid);
    rememberIdentity(db, feed.domain, moduleProject, feed.module_row_id, contextId);
    return contextId;
}

function translateMemoryReferences(db: Database, page: ChangefeedPage): void {
    for (const feed of page.rows) {
        if (feed.domain !== "memories" || feed.op === "tombstone") continue;
        const row = feed.full_row_snapshot;
        const moduleProject = rowString(row, "project_path");
        const reference = row.superseded_by_memory_id;
        const contextReference =
            typeof reference === "number"
                ? (mirrorIdentity(db, "memories", moduleProject, reference)?.context_row_id ?? null)
                : null;
        const mapped = mirrorIdentity(db, "memories", moduleProject, feed.module_row_id);
        if (mapped) {
            db.prepare("UPDATE memories SET superseded_by_memory_id = ? WHERE id = ?").run(
                contextReference,
                mapped.context_row_id,
            );
        }
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
        return;
    }
    const contextId = contextNoteId(db, feed, moduleProject);
    db.prepare(
        `UPDATE notes SET type = 'smart', status = ?, project_path = ?, session_id = ?, content = ?,
         surface_condition = ?, anchor_block_id = ?, created_at = ?, updated_at = ? WHERE id = ?`,
    ).run(
        rowString(row, "status", "active") === "dismissed" ? "dismissed" : "active",
        moduleProject,
        rowNullableString(row, "session_id"),
        rowString(row, "content"),
        rowNullableString(row, "surface_condition"),
        rowNullableString(row, "anchor_block_id"),
        rowNumber(row, "created_at_ms"),
        rowNumber(row, "updated_at_ms"),
        contextId,
    );
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
            for (const feed of page.rows) {
                if (feed.domain !== page.domain || feed.feed_seq <= nextCursor) continue;
                if (feed.domain === "memories") applyMemoryRow(db, feed);
                else applyNoteRow(db, feed);
                nextCursor = feed.feed_seq;
            }
            translateMemoryReferences(db, page);
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
