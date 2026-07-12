import {
    cleanUserText,
    extractTexts,
    hasMeaningfulUserText,
} from "../../hooks/magic-context/read-session-chunk";
import type { RawMessage } from "../../hooks/magic-context/read-session-raw";
import { getHarness } from "../../shared/harness";
import type { Database, Statement as PreparedStatement } from "../../shared/sqlite";
import { removeSystemReminders } from "../../shared/system-directive";
import { clearCompressionDepth } from "./compression-depth-storage";

interface MessageHistoryIndexRow {
    last_indexed_ordinal?: number;
    dirty_floor_ordinal?: number;
}

const lastIndexedStatements = new WeakMap<Database, PreparedStatement>();
const insertMessageStatements = new WeakMap<Database, PreparedStatement>();
const upsertProgressStatements = new WeakMap<Database, PreparedStatement>();
const upsertDirtyFloorStatements = new WeakMap<Database, PreparedStatement>();
const deleteFtsStatements = new WeakMap<Database, PreparedStatement>();
const deleteFtsRangeStatements = new WeakMap<Database, PreparedStatement>();
const deleteIndexStatements = new WeakMap<Database, PreparedStatement>();
const countIndexedMessageStatements = new WeakMap<Database, PreparedStatement>();

function normalizeIndexText(text: string): string {
    return text.replace(/\s+/g, " ").trim();
}

function getLastIndexedStatement(db: Database): PreparedStatement {
    let stmt = lastIndexedStatements.get(db);
    if (!stmt) {
        stmt = db.prepare(
            "SELECT last_indexed_ordinal, dirty_floor_ordinal FROM message_history_index WHERE session_id = ?",
        );
        lastIndexedStatements.set(db, stmt);
    }
    return stmt;
}

function getInsertMessageStatement(db: Database): PreparedStatement {
    let stmt = insertMessageStatements.get(db);
    if (!stmt) {
        stmt = db.prepare(
            "INSERT INTO message_history_fts (session_id, message_ordinal, message_id, role, content) VALUES (?, ?, ?, ?, ?)",
        );
        insertMessageStatements.set(db, stmt);
    }
    return stmt;
}

function getUpsertProgressStatement(db: Database): PreparedStatement {
    let stmt = upsertProgressStatements.get(db);
    if (!stmt) {
        stmt = db.prepare(
            "INSERT INTO message_history_index (session_id, last_indexed_ordinal, dirty_floor_ordinal, updated_at, harness) VALUES (?, ?, ?, ?, ?) ON CONFLICT(session_id) DO UPDATE SET last_indexed_ordinal = excluded.last_indexed_ordinal, dirty_floor_ordinal = excluded.dirty_floor_ordinal, updated_at = excluded.updated_at",
        );
        upsertProgressStatements.set(db, stmt);
    }
    return stmt;
}

function getUpsertDirtyFloorStatement(db: Database): PreparedStatement {
    let stmt = upsertDirtyFloorStatements.get(db);
    if (!stmt) {
        stmt = db.prepare(
            "INSERT INTO message_history_index (session_id, last_indexed_ordinal, dirty_floor_ordinal, updated_at, harness) VALUES (?, ?, ?, ?, ?) ON CONFLICT(session_id) DO UPDATE SET last_indexed_ordinal = MAX(message_history_index.last_indexed_ordinal, excluded.last_indexed_ordinal), dirty_floor_ordinal = CASE WHEN message_history_index.dirty_floor_ordinal <= 0 THEN excluded.dirty_floor_ordinal WHEN excluded.dirty_floor_ordinal <= 0 THEN message_history_index.dirty_floor_ordinal ELSE MIN(message_history_index.dirty_floor_ordinal, excluded.dirty_floor_ordinal) END, updated_at = excluded.updated_at",
        );
        upsertDirtyFloorStatements.set(db, stmt);
    }
    return stmt;
}

function getDeleteFtsStatement(db: Database): PreparedStatement {
    let stmt = deleteFtsStatements.get(db);
    if (!stmt) {
        stmt = db.prepare("DELETE FROM message_history_fts WHERE session_id = ?");
        deleteFtsStatements.set(db, stmt);
    }
    return stmt;
}

function getDeleteFtsRangeStatement(db: Database): PreparedStatement {
    let stmt = deleteFtsRangeStatements.get(db);
    if (!stmt) {
        stmt = db.prepare(
            "DELETE FROM message_history_fts WHERE session_id = ? AND CAST(message_ordinal AS INTEGER) BETWEEN ? AND ?",
        );
        deleteFtsRangeStatements.set(db, stmt);
    }
    return stmt;
}

function getDeleteIndexStatement(db: Database): PreparedStatement {
    let stmt = deleteIndexStatements.get(db);
    if (!stmt) {
        stmt = db.prepare("DELETE FROM message_history_index WHERE session_id = ?");
        deleteIndexStatements.set(db, stmt);
    }
    return stmt;
}

function getCountIndexedMessageStatement(db: Database): PreparedStatement {
    let stmt = countIndexedMessageStatements.get(db);
    if (!stmt) {
        stmt = db.prepare(
            "SELECT COUNT(*) AS count FROM message_history_fts WHERE session_id = ? AND message_id = ?",
        );
        countIndexedMessageStatements.set(db, stmt);
    }
    return stmt;
}

interface CountRow {
    count: number;
}

export function getLastIndexedOrdinal(db: Database, sessionId: string): number {
    const row = getLastIndexedStatement(db).get(sessionId) as MessageHistoryIndexRow | null;
    return typeof row?.last_indexed_ordinal === "number" ? row.last_indexed_ordinal : 0;
}

export function getDirtyIndexFloor(db: Database, sessionId: string): number | null {
    const row = getLastIndexedStatement(db).get(sessionId) as MessageHistoryIndexRow | null;
    return typeof row?.dirty_floor_ordinal === "number" && row.dirty_floor_ordinal > 0
        ? row.dirty_floor_ordinal
        : null;
}

/**
 * Persist the earliest ordinal that an incremental write could leave missing.
 * Callers set this before the FTS transaction so a crash or write failure leaves
 * a durable reconciliation floor instead of an uncovered watermark.
 */
export function markMessageIndexDirty(db: Database, sessionId: string, floorOrdinal: number): void {
    const dirtyFloor = Math.max(1, Math.floor(floorOrdinal));
    getUpsertDirtyFloorStatement(db).run(
        sessionId,
        getLastIndexedOrdinal(db, sessionId),
        dirtyFloor,
        Date.now(),
        getHarness(),
    );
}

function isMessageAlreadyIndexed(db: Database, sessionId: string, messageId: string): boolean {
    const row = getCountIndexedMessageStatement(db).get(sessionId, messageId) as CountRow | null;
    return (typeof row?.count === "number" ? row.count : 0) > 0;
}

function setIndexProgress(
    db: Database,
    sessionId: string,
    watermark: number,
    dirtyFloor: number | null,
    now: number,
): void {
    getUpsertProgressStatement(db).run(
        sessionId,
        Math.max(0, Math.floor(watermark)),
        dirtyFloor ?? 0,
        now,
        getHarness(),
    );
}

export function getMessageIndexReconciliationStartOrdinal(db: Database, sessionId: string): number {
    const watermark = getLastIndexedOrdinal(db, sessionId);
    const dirtyFloor = getDirtyIndexFloor(db, sessionId);
    return dirtyFloor === null ? watermark : Math.min(watermark, dirtyFloor - 1);
}

export function isMessageIndexReconciledThrough(
    db: Database,
    sessionId: string,
    finalWatermark: number,
): boolean {
    const dirtyFloor = getDirtyIndexFloor(db, sessionId);
    return getLastIndexedOrdinal(db, sessionId) >= finalWatermark && dirtyFloor === null;
}

export function deleteIndexedMessage(db: Database, sessionId: string, messageId: string): number {
    const row = getCountIndexedMessageStatement(db).get(sessionId, messageId) as CountRow | null;
    const count = typeof row?.count === "number" ? row.count : 0;

    // Full reindex on next search: ordinals are positional (not stable IDs), so removing
    // a message shifts all subsequent ordinals. Keeping a stale tracker would cause
    // ensureMessagesIndexed() to skip newly added messages when the count matches.
    // Clearing both FTS rows and the tracker forces a complete rebuild on next search.
    clearIndexedMessages(db, sessionId);
    return count;
}

export function clearIndexedMessages(db: Database, sessionId: string): void {
    db.transaction(() => {
        getDeleteFtsStatement(db).run(sessionId);
        getDeleteIndexStatement(db).run(sessionId);
        clearCompressionDepth(db, sessionId);
    })();
}

export function getIndexableContent(role: string, parts: unknown[]): string {
    if (role === "user") {
        if (!hasMeaningfulUserText(parts)) {
            return "";
        }

        return extractTexts(parts)
            .map(cleanUserText)
            .map(normalizeIndexText)
            .filter((text) => text.length > 0)
            .join(" / ");
    }

    if (role === "assistant") {
        return extractTexts(parts)
            .map(removeSystemReminders)
            .map(normalizeIndexText)
            .filter((text) => text.length > 0)
            .join(" / ");
    }

    return "";
}

function indexSingleMessageInTransaction(
    db: Database,
    sessionId: string,
    message: RawMessage,
    now: number,
): boolean {
    const currentWatermark = getLastIndexedOrdinal(db, sessionId);
    const dirtyFloor = getDirtyIndexFloor(db, sessionId);

    // A live event may only extend the already-covered prefix by one ordinal.
    // Out-of-order events leave their earliest missing ordinal dirty for the
    // paged reconciler instead of moving the watermark across a hole.
    if (
        message.ordinal !== currentWatermark + 1 ||
        (dirtyFloor !== null && dirtyFloor !== message.ordinal)
    ) {
        return false;
    }

    let inserted = false;
    if (message.role === "user" || message.role === "assistant") {
        const content = getIndexableContent(message.role, message.parts);
        if (content.length > 0 && !isMessageAlreadyIndexed(db, sessionId, message.id)) {
            getInsertMessageStatement(db).run(
                sessionId,
                message.ordinal,
                message.id,
                message.role,
                content,
            );
            inserted = true;
        }
    }

    setIndexProgress(db, sessionId, message.ordinal, null, now);
    return inserted;
}

export function indexSingleMessage(db: Database, sessionId: string, message: RawMessage): boolean {
    // BEGIN IMMEDIATE (not a deferred db.transaction): message_history_fts is a
    // plain FTS5 table with NO UNIQUE constraint, and the dedup is the
    // isMessageAlreadyIndexed SELECT inside the body. Under a DEFERRED transaction
    // two processes handling the same terminal message.updated can both pass that
    // SELECT before either inserts → duplicate FTS rows. Taking the writer lock up
    // front serializes them, so the second's in-lock re-check sees the first's
    // insert and skips. Mirrors indexMessagesAfterOrdinal.
    db.exec("BEGIN IMMEDIATE");
    let committed = false;
    try {
        const result = indexSingleMessageInTransaction(db, sessionId, message, Date.now());
        db.exec("COMMIT");
        committed = true;
        return result;
    } finally {
        if (!committed) {
            try {
                db.exec("ROLLBACK");
            } catch {
                // already closed by an earlier failure
            }
        }
    }
}

export function indexMessagesAfterOrdinal(
    db: Database,
    sessionId: string,
    messages: RawMessage[],
    _lastIndexedOrdinal: number,
    finalWatermark: number = messages.length,
): number {
    const now = Date.now();
    let inserted = 0;

    // The writer lock protects both duplicate checks and the progress row. Each
    // caller supplies only one bounded source page, so lock hold time is bounded
    // by that page rather than the full session history.
    db.exec("BEGIN IMMEDIATE");
    let committed = false;
    try {
        const currentWatermark = getLastIndexedOrdinal(db, sessionId);
        const dirtyFloor = getDirtyIndexFloor(db, sessionId);
        const effectiveWatermark =
            dirtyFloor === null
                ? currentWatermark
                : Math.min(currentWatermark, Math.max(0, dirtyFloor - 1));

        if (dirtyFloor !== null && dirtyFloor <= finalWatermark) {
            // Rebuild only the portion represented by this source snapshot. A
            // stale snapshot must never delete newer live rows beyond its end.
            getDeleteFtsRangeStatement(db).run(sessionId, dirtyFloor, finalWatermark);
        }

        const messagesByOrdinal = new Map<number, RawMessage>();
        for (const message of messages) {
            if (message.ordinal > effectiveWatermark && message.ordinal <= finalWatermark) {
                messagesByOrdinal.set(message.ordinal, message);
            }
        }

        let coveredWatermark = effectiveWatermark;
        while (coveredWatermark < finalWatermark && messagesByOrdinal.has(coveredWatermark + 1)) {
            coveredWatermark += 1;
        }

        for (let ordinal = effectiveWatermark + 1; ordinal <= coveredWatermark; ordinal++) {
            const message = messagesByOrdinal.get(ordinal);
            if (!message || (message.role !== "user" && message.role !== "assistant")) {
                continue;
            }
            const content = getIndexableContent(message.role, message.parts);
            if (content.length === 0 || isMessageAlreadyIndexed(db, sessionId, message.id)) {
                continue;
            }
            getInsertMessageStatement(db).run(
                sessionId,
                message.ordinal,
                message.id,
                message.role,
                content,
            );
            inserted += 1;
        }

        const missingFloor = coveredWatermark < finalWatermark ? coveredWatermark + 1 : null;
        const preservedFloor =
            dirtyFloor !== null && dirtyFloor > finalWatermark ? dirtyFloor : null;
        const nextDirtyFloor =
            missingFloor === null
                ? preservedFloor
                : preservedFloor === null
                  ? missingFloor
                  : Math.min(missingFloor, preservedFloor);

        // The FTS watermark advances only over contiguous source ordinals. A
        // dirty floor remains recorded until a source page actually covers it.
        setIndexProgress(db, sessionId, coveredWatermark, nextDirtyFloor, now);
        db.exec("COMMIT");
        committed = true;
    } finally {
        if (!committed) {
            try {
                db.exec("ROLLBACK");
            } catch {
                // already rolled back / no active transaction
            }
        }
    }
    return inserted;
}

export function ensureMessagesIndexed(
    db: Database,
    sessionId: string,
    readMessages: (sessionId: string) => RawMessage[],
): void {
    const messages = readMessages(sessionId);

    if (messages.length === 0) {
        db.transaction(() => clearIndexedMessages(db, sessionId))();
        return;
    }

    let lastIndexedOrdinal = getLastIndexedOrdinal(db, sessionId);
    if (lastIndexedOrdinal > messages.length) {
        db.transaction(() => clearIndexedMessages(db, sessionId))();
        lastIndexedOrdinal = 0;
    }

    if (lastIndexedOrdinal >= messages.length && getDirtyIndexFloor(db, sessionId) === null) {
        return;
    }

    indexMessagesAfterOrdinal(db, sessionId, messages, lastIndexedOrdinal, messages.length);
}
