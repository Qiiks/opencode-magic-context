/// <reference types="bun-types" />

import { describe, expect, it } from "bun:test";

import { runMigrations } from "../../features/magic-context/migrations";
import { initializeDatabase } from "../../features/magic-context/storage-db";
import {
    getActiveTagsBySession,
    getTailHygieneTags,
    getTagsBySession,
    insertTag,
} from "../../features/magic-context/storage-tags";
import { createTagger } from "../../features/magic-context/tagger";
import type { Database as DatabaseType } from "../../shared/sqlite";
import { Database } from "../../shared/sqlite";
import { stripDroppedPlaceholderMessages } from "./strip-content";
import { type MessageLike, tagMessages } from "./tag-messages";
import { measureTailHygiene } from "./tail-hygiene-walk";

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

    it("retires an existing whitespace row from active accounting without wire oscillation", () => {
        const db = openTestDb();
        const sessionId = "ses-whitespace-accounting";
        insertTag(db, sessionId, "assistant-blank:p1", "message", 1, 7, 0, null, 0, null, null, {
            tokenCount: 9,
            inputTokenCount: null,
            reasoningTokenCount: null,
        });
        const served: string[] = [];

        for (let pass = 0; pass < 2; pass += 1) {
            const tagger = createTagger();
            tagger.initFromDb(sessionId, db);
            const message = assistant(
                "assistant-blank",
                [{ type: "text", text: " " }],
                sessionId,
            );
            tagMessages(sessionId, [message], tagger, db);
            served.push(textOf(message));

            const hygiene = measureTailHygiene({
                messages: [message],
                tags: getTailHygieneTags(db, sessionId),
                protectedTags: 0,
            });
            expect(hygiene.u).toBe(0);
            expect(getActiveTagsBySession(db, sessionId)).toEqual([]);
        }

        expect(served).toEqual(["§7§  ", "§7§  "]);
        expect(getTagsBySession(db, sessionId)).toMatchObject([
            { tagNumber: 7, status: "compacted" },
        ]);
    });

    it("never rebinds an inert whitespace tag when real text later occupies that part id", () => {
        const db = openTestDb();
        const sessionId = "ses-whitespace-no-rebind";
        insertTag(db, sessionId, "assistant-changing:p0", "message", 1, 1, 0, null, 0, null, null, {
            tokenCount: 0,
            inputTokenCount: null,
            reasoningTokenCount: null,
        });

        const blank = assistant("assistant-changing", [{ type: "text", text: " " }], sessionId);
        tagMessages(sessionId, [blank], createTagger(), db);
        expect(textOf(blank)).toBe("§1§  ");

        for (let pass = 0; pass < 2; pass += 1) {
            const tagger = createTagger();
            tagger.initFromDb(sessionId, db);
            const real = assistant(
                "assistant-changing",
                [{ type: "text", text: "real answer" }],
                sessionId,
            );
            tagMessages(sessionId, [real], tagger, db);
            expect(textOf(real)).toBe("§2§ real answer");
        }

        expect(
            getTagsBySession(db, sessionId).map((tag) => ({
                tagNumber: tag.tagNumber,
                status: tag.status,
            })),
        ).toEqual([
            { tagNumber: 1, status: "compacted" },
            { tagNumber: 2, status: "active" },
        ]);
    });

    it("keeps leading whitespace before signed thinking byte-identical for both provider shapes", () => {
        for (const providerID of ["anthropic", "github-copilot"]) {
            const db = openTestDb();
            const sessionId = `ses-leading-whitespace-${providerID}`;
            const message = assistant(
                "assistant-leading",
                [
                    { type: "text", text: " " },
                    { type: "thinking", thinking: "signed", signature: "sig" },
                ],
                sessionId,
            );
            const before = JSON.stringify(message.parts);

            tagMessages(sessionId, [message], createTagger(), db);
            stripDroppedPlaceholderMessages([message], providerID);

            expect(JSON.stringify(message.parts)).toBe(before);
            expect(getTagsBySession(db, sessionId)).toEqual([]);
        }
    });

    it("preserves provider-specific wholly blank assistant canonicalization", () => {
        for (const [providerID, expected] of [
            ["anthropic", ""],
            ["github-copilot", "[dropped]"],
        ] as const) {
            const db = openTestDb();
            const sessionId = `ses-wholly-blank-${providerID}`;
            const message = assistant(
                "assistant-wholly-blank",
                [{ type: "text", text: " \t" }],
                sessionId,
            );

            tagMessages(sessionId, [message], createTagger(), db);
            stripDroppedPlaceholderMessages([message], providerID);

            expect(message.parts).toEqual([{ type: "text", text: expected }]);
            expect(getTagsBySession(db, sessionId)).toEqual([]);
        }
    });
});
