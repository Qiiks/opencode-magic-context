import { getHarness } from "../../shared/harness";
import type { Database } from "../../shared/sqlite";

export interface CloneCompartmentRow {
    sequence: number;
    startMessage: number;
    endMessage: number;
    startMessageId: string;
    endMessageId: string;
}

export interface CloneTagRow {
    messageId: string;
    type: string;
    tagNumber: number;
    toolOwnerMessageId: string | null;
}

export interface CloneSessionStateFilter {
    resolveBoundaryOrdinal(messageId: string): number | undefined;
    includeTag(tag: CloneTagRow): boolean;
    includeMessageId(messageId: string): boolean;
    selectPendingPiMarker(
        rawState: string | null,
        copiedCompartments: readonly CloneCompartmentRow[],
    ): string | null;
}

export interface CopySessionStateForCloneResult {
    kind: "migrated" | "destination-not-empty";
    compartmentsCopied: number;
    tagsCopied: number;
    pendingOpsCopied: number;
    pendingMarkerMigrated: boolean;
}

type RawCompartmentRow = {
    sequence: number;
    start_message: number;
    end_message: number;
    start_message_id: string;
    end_message_id: string;
    title: string;
    content: string;
    p1: string | null;
    p2: string | null;
    p3: string | null;
    p4: string | null;
    importance: number;
    episode_type: string | null;
    legacy: number;
    created_at: number;
    harness: string;
};

type RawTagRow = {
    message_id: string;
    type: string;
    status: string;
    byte_size: number;
    tag_number: number;
    harness: string;
    entry_fingerprint: string | null;
    token_count: number | null;
    input_token_count: number | null;
    reasoning_token_count: number | null;
    reasoning_byte_size: number;
    drop_mode: string;
    tool_name: string | null;
    input_byte_size: number;
    caveman_depth: number;
    tool_owner_message_id: string | null;
};

type RawSessionMetaRow = {
    cleared_reasoning_through_tag: number | null;
    tool_reclaim_watermark: number | null;
    pi_stable_id_scheme: number | null;
    stripped_placeholder_ids: string | null;
    stale_reduce_stripped_ids: string | null;
    processed_image_stripped_ids: string | null;
    pending_pi_compaction_marker_state: string | null;
    last_todo_state: string | null;
    todo_synthetic_call_id: string | null;
    todo_synthetic_anchor_message_id: string | null;
    todo_synthetic_state_json: string | null;
};

function runImmediate<T>(db: Database, body: () => T): T {
    db.exec("BEGIN IMMEDIATE");
    let committed = false;
    try {
        const result = body();
        db.exec("COMMIT");
        committed = true;
        return result;
    } finally {
        if (!committed) {
            try {
                db.exec("ROLLBACK");
            } catch {
                // The transaction may already have been rolled back by SQLite.
            }
        }
    }
}

function countRows(db: Database, table: "compartments" | "tags", sessionId: string): number {
    const row = db
        .prepare(`SELECT COUNT(*) AS count FROM ${table} WHERE session_id = ?`)
        .get(sessionId) as { count?: number } | undefined;
    return typeof row?.count === "number" ? row.count : 0;
}

function filterIdBlob(raw: string | null, includeMessageId: (id: string) => boolean): string {
    if (!raw) return "";
    try {
        const value = JSON.parse(raw);
        if (!Array.isArray(value)) return "";
        const filtered = value.filter(
            (id): id is string => typeof id === "string" && includeMessageId(id),
        );
        return filtered.length > 0 ? JSON.stringify(filtered) : "";
    } catch {
        return "";
    }
}

function clampWatermark(value: number | null, maxCopiedTag: number): number {
    if (typeof value !== "number" || !Number.isFinite(value) || value <= 0) return 0;
    return Math.min(Math.floor(value), maxCopiedTag);
}

/**
 * Copy durable session content into a clone under one immediate transaction.
 * The destination guard runs after the write lock is acquired so two plugin
 * processes cannot both observe an empty clone and duplicate its state.
 */
export function copySessionStateForClone(
    db: Database,
    sourceSessionId: string,
    destinationSessionId: string,
    filter: CloneSessionStateFilter,
): CopySessionStateForCloneResult {
    return runImmediate(db, () => {
        if (
            countRows(db, "compartments", destinationSessionId) > 0 ||
            countRows(db, "tags", destinationSessionId) > 0
        ) {
            return {
                kind: "destination-not-empty",
                compartmentsCopied: 0,
                tagsCopied: 0,
                pendingOpsCopied: 0,
                pendingMarkerMigrated: false,
            };
        }

        const sourceCompartments = db
            .prepare(
                `SELECT sequence, start_message, end_message, start_message_id, end_message_id,
                        title, content, p1, p2, p3, p4, importance, episode_type, legacy,
                        created_at, harness
                   FROM compartments WHERE session_id = ? ORDER BY sequence ASC`,
            )
            .all(sourceSessionId) as RawCompartmentRow[];
        const insertCompartment = db.prepare(
            `INSERT INTO compartments
                (session_id, sequence, start_message, end_message, start_message_id,
                 end_message_id, title, content, p1, p2, p3, p4, importance,
                 episode_type, legacy, created_at, harness)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
        );
        const copiedCompartments: CloneCompartmentRow[] = [];
        for (const row of sourceCompartments) {
            const startMessage = filter.resolveBoundaryOrdinal(row.start_message_id);
            const endMessage = filter.resolveBoundaryOrdinal(row.end_message_id);
            if (startMessage === undefined || endMessage === undefined) continue;
            insertCompartment.run(
                destinationSessionId,
                row.sequence,
                startMessage,
                endMessage,
                row.start_message_id,
                row.end_message_id,
                row.title,
                row.content,
                row.p1,
                row.p2,
                row.p3,
                row.p4,
                row.importance,
                row.episode_type,
                row.legacy,
                row.created_at,
                row.harness,
            );
            copiedCompartments.push({
                sequence: row.sequence,
                startMessage,
                endMessage,
                startMessageId: row.start_message_id,
                endMessageId: row.end_message_id,
            });
        }

        const sourceTags = db
            .prepare(
                `SELECT message_id, type, status, byte_size, tag_number, harness,
                        entry_fingerprint, token_count, input_token_count,
                        reasoning_token_count, reasoning_byte_size, drop_mode, tool_name,
                        input_byte_size, caveman_depth, tool_owner_message_id
                   FROM tags WHERE session_id = ? ORDER BY tag_number ASC`,
            )
            .all(sourceSessionId) as RawTagRow[];
        const insertTag = db.prepare(
            `INSERT INTO tags
                (session_id, message_id, type, status, byte_size, tag_number, harness,
                 entry_fingerprint, token_count, input_token_count, reasoning_token_count,
                 reasoning_byte_size, drop_mode, tool_name, input_byte_size,
                 caveman_depth, tool_owner_message_id)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
        );
        const copiedTagNumbers: number[] = [];
        for (const row of sourceTags) {
            if (
                !filter.includeTag({
                    messageId: row.message_id,
                    type: row.type,
                    tagNumber: row.tag_number,
                    toolOwnerMessageId: row.tool_owner_message_id,
                })
            ) {
                continue;
            }
            insertTag.run(
                destinationSessionId,
                row.message_id,
                row.type,
                row.status,
                row.byte_size,
                row.tag_number,
                row.harness,
                row.entry_fingerprint,
                row.token_count,
                row.input_token_count,
                row.reasoning_token_count,
                row.reasoning_byte_size,
                row.drop_mode,
                row.tool_name,
                row.input_byte_size,
                row.caveman_depth,
                row.tool_owner_message_id,
            );
            copiedTagNumbers.push(row.tag_number);
        }

        if (copiedTagNumbers.length > 0) {
            const placeholders = copiedTagNumbers.map(() => "?").join(", ");
            db.prepare(
                `INSERT INTO source_contents (tag_id, session_id, content, created_at, harness)
                 SELECT tag_id, ?, content, created_at, harness
                   FROM source_contents
                  WHERE session_id = ? AND tag_id IN (${placeholders})`,
            ).run(destinationSessionId, sourceSessionId, ...copiedTagNumbers);
            db.prepare(
                `INSERT INTO pending_ops (session_id, tag_id, operation, queued_at, harness)
                 SELECT ?, tag_id, operation, queued_at, harness
                   FROM pending_ops
                  WHERE session_id = ? AND tag_id IN (${placeholders})`,
            ).run(destinationSessionId, sourceSessionId, ...copiedTagNumbers);
        }

        const meta = db
            .prepare(
                `SELECT cleared_reasoning_through_tag, tool_reclaim_watermark,
                        pi_stable_id_scheme, stripped_placeholder_ids,
                        stale_reduce_stripped_ids, processed_image_stripped_ids,
                        pending_pi_compaction_marker_state, last_todo_state,
                        todo_synthetic_call_id, todo_synthetic_anchor_message_id,
                        todo_synthetic_state_json
                   FROM session_meta WHERE session_id = ?`,
            )
            .get(sourceSessionId) as RawSessionMetaRow | undefined;
        const maxCopiedTag = copiedTagNumbers.reduce((max, value) => Math.max(max, value), 0);
        const pendingMarker = filter.selectPendingPiMarker(
            meta?.pending_pi_compaction_marker_state ?? null,
            copiedCompartments,
        );
        const todoAnchor = meta?.todo_synthetic_anchor_message_id ?? "";
        const migrateTodo = todoAnchor.length > 0 && filter.includeMessageId(todoAnchor);

        db.prepare(
            `INSERT INTO session_meta
                (session_id, harness, counter, cleared_reasoning_through_tag,
                 tool_reclaim_watermark, pi_stable_id_scheme, stripped_placeholder_ids,
                 stale_reduce_stripped_ids, processed_image_stripped_ids,
                 pending_pi_compaction_marker_state, last_todo_state,
                 todo_synthetic_call_id, todo_synthetic_anchor_message_id,
                 todo_synthetic_state_json)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(session_id) DO UPDATE SET
                harness = excluded.harness,
                counter = excluded.counter,
                cleared_reasoning_through_tag = excluded.cleared_reasoning_through_tag,
                tool_reclaim_watermark = excluded.tool_reclaim_watermark,
                pi_stable_id_scheme = excluded.pi_stable_id_scheme,
                stripped_placeholder_ids = excluded.stripped_placeholder_ids,
                stale_reduce_stripped_ids = excluded.stale_reduce_stripped_ids,
                processed_image_stripped_ids = excluded.processed_image_stripped_ids,
                pending_pi_compaction_marker_state = excluded.pending_pi_compaction_marker_state,
                last_todo_state = excluded.last_todo_state,
                todo_synthetic_call_id = excluded.todo_synthetic_call_id,
                todo_synthetic_anchor_message_id = excluded.todo_synthetic_anchor_message_id,
                todo_synthetic_state_json = excluded.todo_synthetic_state_json,
                cached_m0_bytes = NULL,
                cached_m1_bytes = NULL`,
        ).run(
            destinationSessionId,
            getHarness(),
            maxCopiedTag,
            clampWatermark(meta?.cleared_reasoning_through_tag ?? null, maxCopiedTag),
            clampWatermark(meta?.tool_reclaim_watermark ?? null, maxCopiedTag),
            typeof meta?.pi_stable_id_scheme === "number" &&
                Number.isFinite(meta.pi_stable_id_scheme)
                ? meta.pi_stable_id_scheme
                : null,
            filterIdBlob(meta?.stripped_placeholder_ids ?? null, filter.includeMessageId),
            filterIdBlob(meta?.stale_reduce_stripped_ids ?? null, filter.includeMessageId),
            filterIdBlob(meta?.processed_image_stripped_ids ?? null, filter.includeMessageId),
            pendingMarker,
            migrateTodo ? (meta?.last_todo_state ?? "") : "",
            migrateTodo ? (meta?.todo_synthetic_call_id ?? "") : "",
            migrateTodo ? todoAnchor : "",
            migrateTodo ? (meta?.todo_synthetic_state_json ?? "") : "",
        );

        const pendingOpsRow = db
            .prepare("SELECT COUNT(*) AS count FROM pending_ops WHERE session_id = ?")
            .get(destinationSessionId) as { count?: number } | undefined;
        return {
            kind: "migrated",
            compartmentsCopied: copiedCompartments.length,
            tagsCopied: copiedTagNumbers.length,
            pendingOpsCopied: typeof pendingOpsRow?.count === "number" ? pendingOpsRow.count : 0,
            pendingMarkerMigrated: pendingMarker !== null,
        };
    });
}
