import type { Database } from "../../../shared/sqlite";
import { getPendingSmartNotes, type Note, type NoteCheckStatus } from "../storage-notes";
import {
    SMART_NOTE_CHECK_LIVENESS_RECHECK_MS,
    SMART_NOTE_CHECK_MAX_STALENESS_MS,
    SMART_NOTE_CHECK_POLICY_VERSION,
    type SmartNoteCheckManifest,
    type SmartNoteCheckNote,
} from "./types";

function toSmartNote(note: Note): SmartNoteCheckNote {
    return {
        ...note,
        checkStatus: note.checkStatus ?? "uncompiled",
        checkFailureCount: note.checkFailureCount ?? 0,
        checkNetworkFailureCount: note.checkNetworkFailureCount ?? 0,
        policyVersion: note.policyVersion ?? 0,
    };
}

export function commitSmartNoteState(
    db: Database,
    args: {
        phase: string;
        leaseHeld?: () => boolean;
        write: () => void;
    },
): void {
    // BEGIN IMMEDIATE, not a plain (deferred) transaction: the lease check reads
    // before the write, and a deferred read→write lock upgrade returns
    // SQLITE_BUSY immediately when another writer holds the lock — busy_timeout
    // does not apply to upgrades, so concurrent processes produced spurious
    // "database is locked" failures here. Taking the write lock at BEGIN time
    // waits under busy_timeout like every other writer.
    db.exec("BEGIN IMMEDIATE");
    let leaseLost = false;
    try {
        // State transitions surface notes or change future scheduling. The lease
        // check must be inside the same write transaction so ownership cannot be
        // lost between a pre-check and the durable update.
        if (args.leaseHeld && !args.leaseHeld()) {
            leaseLost = true;
        } else {
            args.write();
        }
        db.exec("COMMIT");
    } catch (error) {
        try {
            db.exec("ROLLBACK");
        } catch {
            // Connection-level failures leave nothing to roll back.
        }
        throw error;
    }
    if (leaseLost) {
        throw new Error(`Dream lease lost during smart-note ${args.phase} commit`);
    }
}

export function getDueCompiledSmartNoteChecks(
    db: Database,
    projectPath: string,
    now: number,
    limit: number,
): SmartNoteCheckNote[] {
    return getPendingSmartNotes(db, projectPath)
        .map(toSmartNote)
        .filter(
            (note) =>
                note.checkStatus === "compiled" &&
                note.compiledCheck !== null &&
                note.policyVersion === SMART_NOTE_CHECK_POLICY_VERSION &&
                (note.checkQuarantinedUntil === null || note.checkQuarantinedUntil <= now) &&
                (note.checkNextDueAt === null || note.checkNextDueAt <= now),
        )
        .sort((a, b) => (a.checkNextDueAt ?? 0) - (b.checkNextDueAt ?? 0) || a.id - b.id)
        .slice(0, Math.max(1, limit));
}

export function getSmartNotesNeedingCompilation(
    db: Database,
    projectPath: string,
    now: number,
    limit: number,
): SmartNoteCheckNote[] {
    return getPendingSmartNotes(db, projectPath)
        .map(toSmartNote)
        .filter(
            (note) =>
                (note.checkNextDueAt === null || note.checkNextDueAt <= now) &&
                (note.checkStatus === "uncompiled" ||
                    note.checkStatus === "failing" ||
                    note.compiledCheck === null ||
                    note.policyVersion !== SMART_NOTE_CHECK_POLICY_VERSION),
        )
        .sort((a, b) => a.createdAt - b.createdAt || a.id - b.id)
        .slice(0, Math.max(1, limit));
}

export function getStaleCompiledSmartNotes(
    db: Database,
    projectPath: string,
    now: number,
    limit: number,
): SmartNoteCheckNote[] {
    const staleBefore = now - SMART_NOTE_CHECK_MAX_STALENESS_MS;
    const livenessBefore = now - SMART_NOTE_CHECK_LIVENESS_RECHECK_MS;
    return getPendingSmartNotes(db, projectPath)
        .map(toSmartNote)
        .filter(
            (note) =>
                note.checkStatus === "compiled" &&
                note.compiledCheck !== null &&
                note.policyVersion === SMART_NOTE_CHECK_POLICY_VERSION &&
                note.checkFalseSinceAt !== null &&
                note.checkFalseSinceAt <= staleBefore &&
                (note.checkLastLivenessAt === null || note.checkLastLivenessAt <= livenessBefore),
        )
        .sort((a, b) => (a.checkFalseSinceAt ?? 0) - (b.checkFalseSinceAt ?? 0) || a.id - b.id)
        .slice(0, Math.max(1, limit));
}

export function storeCompiledSmartNoteCheck(
    db: Database,
    args: {
        noteId: number;
        compiledCheck: string;
        manifest: SmartNoteCheckManifest;
        checkHash: string;
        checkCron: string;
        nextDueAt: number;
        now: number;
    },
): void {
    db.prepare(
        `UPDATE notes
         SET compiled_check = ?,
             manifest_json = ?,
             check_hash = ?,
             check_cron = ?,
             check_version = 1,
             check_status = 'compiled',
             check_failure_count = 0,
             check_network_failure_count = 0,
             check_quarantined_until = NULL,
             check_next_due_at = ?,
             check_compiled_at = ?,
             check_false_since_at = COALESCE(check_false_since_at, ?),
             check_last_liveness_at = NULL,
             policy_version = ?,
             updated_at = ?
         WHERE id = ? AND type = 'smart'`,
    ).run(
        args.compiledCheck,
        JSON.stringify(args.manifest),
        args.checkHash,
        args.checkCron,
        args.nextDueAt,
        args.now,
        args.now,
        SMART_NOTE_CHECK_POLICY_VERSION,
        args.now,
        args.noteId,
    );
}

export function markCompiledCheckFalse(
    db: Database,
    noteId: number,
    nextDueAt: number,
    now: number,
): void {
    db.prepare(
        `UPDATE notes
         SET last_checked_at = ?,
             updated_at = ?,
             check_next_due_at = ?,
             check_failure_count = 0,
             check_network_failure_count = 0,
             check_false_since_at = COALESCE(check_false_since_at, ?)
         WHERE id = ? AND type = 'smart'`,
    ).run(now, now, nextDueAt, now, noteId);
}

export function markCompiledCheckLogicFailure(
    db: Database,
    noteId: number,
    now: number,
    maxFailures: number,
): void {
    const failureCount = readFailureCount(db, noteId, "check_failure_count") + 1;
    const status: NoteCheckStatus = failureCount >= maxFailures ? "failing" : "compiled";
    db.prepare(
        `UPDATE notes
         SET check_failure_count = ?,
             check_status = ?,
             check_next_due_at = ?,
             updated_at = ?
         WHERE id = ? AND type = 'smart'`,
    ).run(failureCount, status, now + backoffMs(failureCount), now, noteId);
}

export function markCompiledCheckNetworkFailure(
    db: Database,
    noteId: number,
    now: number,
    maxFailures: number,
): void {
    const failureCount = readFailureCount(db, noteId, "check_network_failure_count") + 1;
    const quarantinedUntil = now + backoffMs(failureCount);
    const status: NoteCheckStatus = failureCount >= maxFailures ? "failing" : "compiled";
    db.prepare(
        `UPDATE notes
         SET check_network_failure_count = ?,
             check_status = ?,
             check_next_due_at = ?,
             check_quarantined_until = ?,
             updated_at = ?
         WHERE id = ? AND type = 'smart'`,
    ).run(failureCount, status, quarantinedUntil, quarantinedUntil, now, noteId);
}

export function markSmartNoteLivenessChecked(db: Database, noteId: number, now: number): void {
    db.prepare(
        `UPDATE notes
         SET check_last_liveness_at = ?, updated_at = ?
         WHERE id = ? AND type = 'smart'`,
    ).run(now, now, noteId);
}

export function markSmartNoteCheckStatus(
    db: Database,
    noteId: number,
    status: NoteCheckStatus,
    now: number,
): void {
    db.prepare(
        `UPDATE notes SET check_status = ?, updated_at = ? WHERE id = ? AND type = 'smart'`,
    ).run(status, now, noteId);
}

export function markSmartNoteCompilationFailure(
    db: Database,
    noteId: number,
    now: number,
    maxFailures: number,
): void {
    const failureCount = readFailureCount(db, noteId, "check_failure_count") + 1;
    const status: NoteCheckStatus = failureCount >= maxFailures ? "fallback" : "uncompiled";
    db.prepare(
        `UPDATE notes
         SET check_failure_count = ?,
             check_status = ?,
             check_next_due_at = ?,
             updated_at = ?
         WHERE id = ? AND type = 'smart'`,
    ).run(failureCount, status, now + backoffMs(failureCount), now, noteId);
}

function readFailureCount(db: Database, noteId: number, column: string): number {
    if (column !== "check_failure_count" && column !== "check_network_failure_count") return 0;
    const row = db.prepare(`SELECT ${column} AS count FROM notes WHERE id = ?`).get(noteId) as
        | { count?: number | null }
        | undefined;
    return row?.count ?? 0;
}

function backoffMs(failureCount: number): number {
    const minutes = Math.min(24 * 60, 5 * 2 ** Math.max(0, failureCount - 1));
    return minutes * 60 * 1000;
}
