import { afterEach, describe, expect, it } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { closeDatabase, openDatabase } from "./storage";
import {
    listEmbeddingMeasurements,
    normalizedQueryHash,
    recordEmbeddingMeasurement,
} from "./storage-embedding-measurements";

describe("embedding measurement corpus", () => {
    const dirs: string[] = [];
    const original = process.env.XDG_DATA_HOME;

    afterEach(() => {
        closeDatabase();
        process.env.XDG_DATA_HOME = original;
        for (const dir of dirs.splice(0)) rmSync(dir, { recursive: true, force: true });
    });

    it("stores hashed, bounded rank lists once per query cohort", () => {
        const dir = mkdtempSync(join(tmpdir(), "embedding-measurements-"));
        dirs.push(dir);
        process.env.XDG_DATA_HOME = dir;
        const db = openDatabase();
        const input = {
            sessionId: "ses-measure",
            projectPath: "/repo",
            queryText: "  Queue   backpressure ",
            cohortKey: "fp-a:0|fp-b:0",
            primaryResultIds: Array.from({ length: 12 }, (_, index) => `p:${index}`),
            shadowResultIds: ["s:1"],
            primaryLatencyMs: 10,
            shadowLatencyMs: 20,
            primaryFailed: false,
            shadowFailed: false,
            primaryModelId: "local-id",
            shadowModelId: "synapse-id",
            primaryFingerprint: "",
            shadowFingerprint: "fp-b",
            primaryEpoch: 0,
            shadowEpoch: 0,
            corpusHash: "corpus",
            coverage: { primary: 12, shadow: 1 },
        } as const;

        expect(recordEmbeddingMeasurement(db, input)).toBe(true);
        expect(recordEmbeddingMeasurement(db, input)).toBe(false);
        const rows = listEmbeddingMeasurements(db, "ses-measure");
        expect(rows).toHaveLength(1);
        expect(rows[0].query_text_hash).toBe(normalizedQueryHash(input.queryText));
        expect(JSON.parse(rows[0].primary_result_ids_json)).toHaveLength(10);
        expect(rows[0].query_text_hash).not.toContain(input.queryText);
    });
});
