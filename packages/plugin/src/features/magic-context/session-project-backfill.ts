import { existsSync } from "node:fs";
import { getHarness } from "../../shared/harness";
import { log } from "../../shared/logger";
import type { Database } from "../../shared/sqlite";
import { resolveProjectIdentity } from "./memory/project-identity";
import { recordSessionProjectIdentity } from "./session-project-storage";

const LEASE_TTL_MS = 10 * 60 * 1000;
const SESSION_QUERY_CHUNK_SIZE = 250;
const YIELD_EVERY_NEW_DIRECTORIES = 10;

export interface SessionProjectBackfillSession {
    sessionId: string;
    directory: string;
}

export interface BackfillResult {
    status: "completed" | "already_completed" | "blocked_by_lease";
    totalSessions: number;
    alreadyMappedSessions: number;
    unmappedSessions: number;
    backfilledSessions: number;
    skippedDeadDirectories: number;
    skippedEmptyDirectories: number;
    durationMs: number;
}

export interface SessionProjectBackfillStateRow {
    harness: string;
    status: "running" | "completed";
    started_at: number | null;
    lease_expires_at: number | null;
    completed_at: number | null;
}

interface RunSessionProjectBackfillOptions {
    resolveIdentity?: (directory: string) => string;
    now?: () => number;
    yieldFn?: () => Promise<void>;
}

function ensureBackfillStateTable(db: Database): void {
    db.exec(`
        CREATE TABLE IF NOT EXISTS session_project_backfill_state (
            harness TEXT PRIMARY KEY,
            status TEXT NOT NULL CHECK (status IN ('running', 'completed')),
            started_at INTEGER,
            lease_expires_at INTEGER,
            completed_at INTEGER
        );
    `);
}

function withImmediateTransaction<T>(db: Database, fn: () => T): T {
    db.exec("BEGIN IMMEDIATE");
    try {
        const result = fn();
        db.exec("COMMIT");
        return result;
    } catch (error) {
        try {
            db.exec("ROLLBACK");
        } catch {
            // Ignore rollback failures: the original error is the actionable one.
        }
        throw error;
    }
}

function defaultYieldFn(): Promise<void> {
    return new Promise((resolve) => setTimeout(resolve, 0));
}

function acquireBackfillLease(
    db: Database,
    harness: string,
    now: number,
): BackfillResult["status"] {
    ensureBackfillStateTable(db);
    return withImmediateTransaction(db, () => {
        const current = db
            .prepare(
                `SELECT harness, status, started_at, lease_expires_at, completed_at
                 FROM session_project_backfill_state
                 WHERE harness = ?`,
            )
            .get(harness) as SessionProjectBackfillStateRow | null | undefined;

        if (current?.status === "completed") {
            return "already_completed";
        }

        if (
            current?.status === "running" &&
            current.lease_expires_at !== null &&
            current.lease_expires_at > now
        ) {
            return "blocked_by_lease";
        }

        db.prepare(
            `INSERT INTO session_project_backfill_state(
                harness,
                status,
                started_at,
                lease_expires_at,
                completed_at
            )
             VALUES (?, 'running', ?, ?, NULL)
             ON CONFLICT(harness) DO UPDATE SET
                status = 'running',
                started_at = excluded.started_at,
                lease_expires_at = excluded.lease_expires_at,
                completed_at = NULL`,
        ).run(harness, now, now + LEASE_TTL_MS);

        return "completed";
    });
}

function renewBackfillLease(db: Database, harness: string, now: number): void {
    ensureBackfillStateTable(db);
    db.prepare(
        `UPDATE session_project_backfill_state
         SET lease_expires_at = ?
         WHERE harness = ? AND status = 'running'`,
    ).run(now + LEASE_TTL_MS, harness);
}

function markBackfillCompleted(db: Database, harness: string, now: number): void {
    ensureBackfillStateTable(db);
    db.prepare(
        `INSERT INTO session_project_backfill_state(
            harness,
            status,
            started_at,
            lease_expires_at,
            completed_at
        )
         VALUES (?, 'completed', ?, NULL, ?)
         ON CONFLICT(harness) DO UPDATE SET
            status = 'completed',
            lease_expires_at = NULL,
            completed_at = excluded.completed_at`,
    ).run(harness, now, now);
}

function dedupeSessions(
    sessions: readonly SessionProjectBackfillSession[],
): SessionProjectBackfillSession[] {
    const sessionsById = new Map<string, SessionProjectBackfillSession>();
    for (const session of sessions) {
        if (!session.sessionId) continue;
        const existing = sessionsById.get(session.sessionId);
        if (!existing) {
            sessionsById.set(session.sessionId, {
                sessionId: session.sessionId,
                directory: session.directory,
            });
            continue;
        }
        if (!existing.directory && session.directory) {
            sessionsById.set(session.sessionId, {
                sessionId: session.sessionId,
                directory: session.directory,
            });
        }
    }
    return [...sessionsById.values()];
}

function getMappedSessionIds(
    db: Database,
    harness: string,
    sessions: readonly SessionProjectBackfillSession[],
): Set<string> {
    const mapped = new Set<string>();
    for (let index = 0; index < sessions.length; index += SESSION_QUERY_CHUNK_SIZE) {
        const chunk = sessions.slice(index, index + SESSION_QUERY_CHUNK_SIZE);
        if (chunk.length === 0) continue;
        const placeholders = chunk.map(() => "?").join(", ");
        const rows = db
            .prepare(
                `SELECT session_id
                 FROM session_projects
                 WHERE harness = ?
                   AND session_id IN (${placeholders})`,
            )
            .all(harness, ...chunk.map((session) => session.sessionId)) as Array<{
            session_id: string;
        }>;
        for (const row of rows) {
            mapped.add(row.session_id);
        }
    }
    return mapped;
}

export async function runSessionProjectBackfill(
    db: Database,
    sessions: readonly SessionProjectBackfillSession[],
    options: RunSessionProjectBackfillOptions = {},
): Promise<BackfillResult> {
    const startedAt = performance.now();
    const harness = getHarness();
    const now = options.now ?? Date.now;
    const resolveIdentity = options.resolveIdentity ?? resolveProjectIdentity;
    const yieldFn = options.yieldFn ?? defaultYieldFn;
    const dedupedSessions = dedupeSessions(sessions);

    const result: BackfillResult = {
        status: "completed",
        totalSessions: dedupedSessions.length,
        alreadyMappedSessions: 0,
        unmappedSessions: 0,
        backfilledSessions: 0,
        skippedDeadDirectories: 0,
        skippedEmptyDirectories: 0,
        durationMs: 0,
    };

    const leaseStatus = acquireBackfillLease(db, harness, now());
    if (leaseStatus !== "completed") {
        result.status = leaseStatus;
        result.durationMs = performance.now() - startedAt;
        return result;
    }

    const mappedSessionIds = getMappedSessionIds(db, harness, dedupedSessions);
    result.alreadyMappedSessions = mappedSessionIds.size;

    const existenceCache = new Map<string, boolean>();
    const identityCache = new Map<string, string>();
    let newDirectoryResolutions = 0;

    for (const session of dedupedSessions) {
        if (mappedSessionIds.has(session.sessionId)) {
            continue;
        }
        result.unmappedSessions += 1;

        if (!session.directory) {
            result.skippedEmptyDirectories += 1;
            continue;
        }

        const stillExists = existenceCache.get(session.directory) ?? existsSync(session.directory);
        existenceCache.set(session.directory, stillExists);
        if (!stillExists) {
            result.skippedDeadDirectories += 1;
            continue;
        }

        let identity = identityCache.get(session.directory);
        if (!identity) {
            identity = resolveIdentity(session.directory);
            identityCache.set(session.directory, identity);
            newDirectoryResolutions += 1;
            if (newDirectoryResolutions % YIELD_EVERY_NEW_DIRECTORIES === 0) {
                renewBackfillLease(db, harness, now());
                await yieldFn();
                renewBackfillLease(db, harness, now());
            }
        }

        recordSessionProjectIdentity(db, session.sessionId, identity);
        result.backfilledSessions += 1;
    }

    markBackfillCompleted(db, harness, now());
    result.durationMs = performance.now() - startedAt;
    log(
        `[session-projects] backfilled ${result.backfilledSessions} of ${result.unmappedSessions} unmapped sessions (skipped ${result.skippedDeadDirectories} dead dirs) in ${Math.round(result.durationMs)}ms`,
    );
    return result;
}

export function _getSessionProjectBackfillState(
    db: Database,
    harness = getHarness(),
): SessionProjectBackfillStateRow | null {
    ensureBackfillStateTable(db);
    const row = db
        .prepare("SELECT * FROM session_project_backfill_state WHERE harness = ?")
        .get(harness);
    if (row === null || row === undefined) return null;
    return row as SessionProjectBackfillStateRow;
}
