/// <reference types="bun-types" />

import { afterEach, beforeEach, describe, expect, it } from "bun:test";
import { Database } from "../../shared/sqlite";
import { runMigrations } from "./migrations";
import { initializeDatabase } from "./storage-db";
import {
    addMergedReasoningStrippedIds,
    getMergedReasoningStrippedIds,
} from "./storage-meta-persisted";
import { clearSession } from "./storage-meta-session";

describe("merged_reasoning_stripped_ids", () => {
    let db: Database;
    const sessionId = "ses-merged-reasoning";

    beforeEach(() => {
        db = new Database(":memory:");
        initializeDatabase(db);
        runMigrations(db);
    });

    afterEach(() => {
        db.close();
    });

    it("persists a monotonic union of assistant message ids", () => {
        expect(getMergedReasoningStrippedIds(db, sessionId)).toEqual(new Set());

        expect(addMergedReasoningStrippedIds(db, sessionId, ["assistant-1"])).toBe(true);
        expect(addMergedReasoningStrippedIds(db, sessionId, ["assistant-1", "assistant-2"])).toBe(
            true,
        );

        expect(getMergedReasoningStrippedIds(db, sessionId)).toEqual(
            new Set(["assistant-1", "assistant-2"]),
        );
    });

    it("removes the applied set when the session is cleared", () => {
        addMergedReasoningStrippedIds(db, sessionId, ["assistant-1"]);
        expect(getMergedReasoningStrippedIds(db, sessionId)).toEqual(new Set(["assistant-1"]));

        clearSession(db, sessionId);

        expect(getMergedReasoningStrippedIds(db, sessionId)).toEqual(new Set());
        expect(
            db.prepare("SELECT 1 FROM session_meta WHERE session_id = ?").get(sessionId),
        ).toBeNull();
    });
});
