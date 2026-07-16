import { createHash } from "node:crypto";
import type { Database } from "../../shared/sqlite";

export interface EmbeddingMeasurementInput {
    sessionId: string;
    projectPath: string;
    queryText: string;
    cohortKey: string;
    primaryResultIds: readonly string[];
    shadowResultIds: readonly string[];
    primaryLatencyMs: number | null;
    shadowLatencyMs: number | null;
    primaryFailed: boolean;
    shadowFailed: boolean;
    primaryModelId: string;
    shadowModelId: string;
    primaryFingerprint: string;
    shadowFingerprint: string;
    primaryEpoch: number;
    shadowEpoch: number;
    corpusHash: string;
    coverage: Record<string, unknown>;
}

export interface EmbeddingMeasurementRow {
    id: number;
    session_id: string;
    project_path: string;
    dedup_key: string;
    cohort_key: string;
    query_text_hash: string;
    primary_result_ids_json: string;
    shadow_result_ids_json: string;
    primary_latency_ms: number | null;
    shadow_latency_ms: number | null;
    primary_failed: number;
    shadow_failed: number;
    primary_model_id: string;
    shadow_model_id: string;
    primary_fingerprint: string;
    shadow_fingerprint: string;
    primary_epoch: number;
    shadow_epoch: number;
    corpus_hash: string;
    coverage_json: string;
    created_at: number;
}

export function normalizedQueryHash(query: string): string {
    const normalized = query.trim().replace(/\s+/g, " ").toLowerCase();
    return createHash("sha256").update(normalized).digest("hex");
}

export function recordEmbeddingMeasurement(
    db: Database,
    input: EmbeddingMeasurementInput,
): boolean {
    const queryTextHash = normalizedQueryHash(input.queryText);
    const dedupKey = queryTextHash;
    const result = db
        .prepare(
            `INSERT OR IGNORE INTO embedding_measurement_corpus
            (session_id, project_path, dedup_key, cohort_key, query_text_hash,
             primary_result_ids_json, shadow_result_ids_json, primary_latency_ms, shadow_latency_ms,
             primary_failed, shadow_failed, primary_model_id, shadow_model_id,
             primary_fingerprint, shadow_fingerprint, primary_epoch, shadow_epoch,
             corpus_hash, coverage_json, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
        )
        .run(
            input.sessionId,
            input.projectPath,
            dedupKey,
            input.cohortKey,
            queryTextHash,
            JSON.stringify(input.primaryResultIds.slice(0, 10)),
            JSON.stringify(input.shadowResultIds.slice(0, 10)),
            input.primaryLatencyMs,
            input.shadowLatencyMs,
            input.primaryFailed ? 1 : 0,
            input.shadowFailed ? 1 : 0,
            input.primaryModelId,
            input.shadowModelId,
            input.primaryFingerprint,
            input.shadowFingerprint,
            input.primaryEpoch,
            input.shadowEpoch,
            input.corpusHash,
            JSON.stringify(input.coverage),
            Date.now(),
        );
    return result.changes > 0;
}

export function listEmbeddingMeasurements(
    db: Database,
    sessionId: string,
): EmbeddingMeasurementRow[] {
    return db
        .prepare("SELECT * FROM embedding_measurement_corpus WHERE session_id = ? ORDER BY id ASC")
        .all(sessionId) as EmbeddingMeasurementRow[];
}

export interface SynapseBatchLedgerInput {
    sessionId: string;
    projectPath: string;
    scope: "memory" | "commit" | "chunk";
    manifest: readonly { id: string; contentSha256: string }[];
    requestKey: string;
}

export function beginSynapseBatchLedger(db: Database, input: SynapseBatchLedgerInput): void {
    const now = Date.now();
    db.prepare(
        `INSERT INTO synapse_batch_ledger
            (session_id, project_path, scope, manifest_json, request_key, status, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, 'pending', ?, ?)
         ON CONFLICT(session_id, request_key) DO UPDATE SET
            manifest_json = excluded.manifest_json,
            updated_at = excluded.updated_at`,
    ).run(
        input.sessionId,
        input.projectPath,
        input.scope,
        JSON.stringify(input.manifest),
        input.requestKey,
        now,
        now,
    );
}

export function finishSynapseBatchLedger(
    db: Database,
    sessionId: string,
    requestKey: string,
    status: "complete" | "partial" | "failed",
): void {
    db.prepare(
        "UPDATE synapse_batch_ledger SET status = ?, updated_at = ? WHERE session_id = ? AND request_key = ?",
    ).run(status, Date.now(), sessionId, requestKey);
}
