import { describe, expect, test } from "bun:test";
import { initializeDatabase } from "../../features/magic-context/storage-db";
import { getOrCreateSessionMeta } from "../../features/magic-context/storage-meta";
import { Database } from "../../shared/sqlite";
import { executeStatus } from "./execute-status";
import { estimateTokens } from "./read-session-formatting";

const SESSION_ID = "ses_execute_status";

describe("executeStatus", () => {
    test("attributes history tokens using rendered compartment headings", () => {
        const db = new Database(":memory:");
        initializeDatabase(db);
        getOrCreateSessionMeta(db, SESSION_ID);
        db.prepare(
            "INSERT INTO compartments (session_id, sequence, start_message, end_message, start_message_id, end_message_id, title, content, created_at) VALUES (?,?,?,?,?,?,?,?,?)",
        ).run(SESSION_ID, 1, 12, 34, "m12", "m34", "Status arc", "status body", Date.now());

        const status = executeStatus(db, SESSION_ID, 20);
        const expected = estimateTokens("## 12-34 · Status arc\nstatus body\n");

        expect(status).toContain(`- History block: ~${expected.toLocaleString()} tokens`);
        db.close();
    });
});
