/// <reference types="bun-types" />

import { describe, expect, it } from "bun:test";

import { runMigrations } from "../../features/magic-context/migrations";
import { initializeDatabase } from "../../features/magic-context/storage-db";
import { insertTag } from "../../features/magic-context/storage-tags";
import { createTagger } from "../../features/magic-context/tagger";
import type { Database as DatabaseType } from "../../shared/sqlite";
import { Database } from "../../shared/sqlite";
import { type MessageLike, tagMessages } from "./tag-messages";

function openTestDb(): DatabaseType {
    const db = new Database(":memory:");
    initializeDatabase(db);
    runMigrations(db);
    return db;
}

function assistant(id: string, parts: unknown[], sessionId: string): MessageLike {
    return {
        info: { id, role: "assistant", sessionID: sessionId },
        parts,
    };
}

function textOf(message: MessageLike, index = 0): string {
    return (message.parts[index] as { text: string }).text;
}

describe("whitespace-only assistant tag transition", () => {
    it("replays an existing whitespace prefix on defer and busting passes", () => {
        const db = openTestDb();
        const sessionId = "ses-whitespace-transition";
        insertTag(db, sessionId, "assistant-blank:p0", "message", 2, 1, 0, null, 0, null, null, {
            tokenCount: 0,
            inputTokenCount: null,
            reasoningTokenCount: null,
        });
        const previousServe = "§1§  \t";

        for (const passClass of ["defer", "bust"] as const) {
            const tagger = createTagger();
            const message = assistant(
                "assistant-blank",
                [{ type: "text", text: " \t" }],
                sessionId,
            );

            tagMessages(sessionId, [message], tagger, db);

            expect(passClass).toMatch(/^(defer|bust)$/);
            expect(textOf(message)).toBe(previousServe);
        }
    });
});
