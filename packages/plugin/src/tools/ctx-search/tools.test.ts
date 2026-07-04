import { afterEach, beforeEach, describe, expect, it, spyOn } from "bun:test";
import { replaceAllCompartments } from "../../features/magic-context/compartment-storage";
import { insertMemory } from "../../features/magic-context/memory";
import { indexMessagesAfterOrdinal } from "../../features/magic-context/message-index";
import { runMigrations } from "../../features/magic-context/migrations";
import type { UnifiedSearchResult } from "../../features/magic-context/search";
import * as searchModule from "../../features/magic-context/search";
import { initializeDatabase } from "../../features/magic-context/storage-db";
import { Database } from "../../shared/sqlite";
import { closeQuietly } from "../../shared/sqlite-helpers";
import { createCtxSearchTools } from "./tools";

const toolContext = (sessionID = "ses-search") => ({ sessionID }) as never;
const EXPAND_HINT =
    "Use ctx_expand(start, end) with the range from any message result above to read the full conversation context.";
const NOTE_EXPAND_HINT =
    "Use ctx_expand(start=N-10, end=N) around any note @msg anchor above to read the surrounding conversation context.";

function createTestDb(): Database {
    const db = new Database(":memory:");
    initializeDatabase(db);
    runMigrations(db);
    return db;
}

describe("createCtxSearchTools", () => {
    let db: Database;

    beforeEach(() => {
        db = createTestDb();
    });

    afterEach(() => {
        closeQuietly(db);
    });

    it("validates required query", async () => {
        const tools = createCtxSearchTools({
            db,
            resolveProjectPath: () => "/repo/project",
            memoryEnabled: false,
            embeddingEnabled: false,
            readMessages: () => [],
        });

        const result = await tools.ctx_search.execute({ query: "   " }, toolContext());

        expect(result).toBe("Error: 'query' is required.");
    });

    it("formats empty search results", async () => {
        const tools = createCtxSearchTools({
            db,
            resolveProjectPath: () => "/repo/project",
            memoryEnabled: false,
            embeddingEnabled: false,
            readMessages: () => [],
        });

        const result = await tools.ctx_search.execute({ query: "missing" }, toolContext());

        expect(result).toContain("No results found");
    });

    it("formats message results with inline ranges and one trailing expand hint", async () => {
        replaceAllCompartments(db, "ses-message", [
            {
                sequence: 1,
                startMessage: 1,
                endMessage: 10,
                startMessageId: "m1",
                endMessageId: "m10",
                title: "Compartment",
                content: "Summary",
            },
        ]);
        const tools = createCtxSearchTools({
            db,
            resolveProjectPath: () => "/repo/project",
            memoryEnabled: false,
            embeddingEnabled: false,
            readMessages: () => [
                {
                    ordinal: 5,
                    id: "m5",
                    role: "assistant",
                    parts: [{ type: "text", text: "Alpha migration details are here." }],
                },
                {
                    ordinal: 6,
                    id: "m6",
                    role: "user",
                    parts: [{ type: "text", text: "More alpha migration context." }],
                },
            ],
        });
        indexMessagesAfterOrdinal(
            db,
            "ses-message",
            [
                {
                    ordinal: 5,
                    id: "m5",
                    role: "assistant",
                    parts: [{ type: "text", text: "Alpha migration details are here." }],
                },
                {
                    ordinal: 6,
                    id: "m6",
                    role: "user",
                    parts: [{ type: "text", text: "More alpha migration context." }],
                },
            ],
            0,
            6,
        );

        const result = await tools.ctx_search.execute(
            { query: "alpha migration", sources: ["message"] },
            toolContext("ses-message"),
        );

        expect(result).toContain("[1] [message] score=1.00 ordinal=6 range=3-9 role=user");
        expect(result).toContain("[2] [message] score=0.50 ordinal=5 range=2-8 role=assistant");
        expect(result.split(EXPAND_HINT).length - 1).toBe(1);
        expect(result.endsWith(EXPAND_HINT)).toBe(true);
        expect(result).not.toContain("Expand with ctx_expand(start=");
    });

    it("omits the consolidated expand hint for memory-only results", async () => {
        insertMemory(db, {
            projectPath: "/repo/project",
            category: "ARCHITECTURE_DECISIONS",
            content: "Alpha memory only search result.",
        });
        const tools = createCtxSearchTools({
            db,
            resolveProjectPath: () => "/repo/project",
            memoryEnabled: true,
            embeddingEnabled: false,
            readMessages: () => [],
        });

        const result = await tools.ctx_search.execute(
            { query: "alpha", sources: ["memory"] },
            toolContext(),
        );

        expect(result).toContain("[1] [memory]");
        expect(result).not.toContain(EXPAND_HINT);
        expect(result).not.toContain("ctx_expand");
    });

    it("formats note results with note ids, status labels, and anchor expand hints", async () => {
        const spy = spyOn(searchModule, "unifiedSearch").mockImplementation(
            async () =>
                [
                    {
                        source: "note",
                        content: "Keep the dry-run fallback until telemetry stabilizes.",
                        score: 0.88,
                        noteId: 7,
                        status: "dismissed",
                        createdAt: Date.now() - 2 * 24 * 60 * 60 * 1000,
                        anchorOrdinal: 44,
                    },
                ] as UnifiedSearchResult[],
        );
        try {
            const tools = createCtxSearchTools({
                db,
                resolveProjectPath: () => "/repo/project",
                memoryEnabled: false,
                embeddingEnabled: false,
                readMessages: () => [],
            });

            const result = await tools.ctx_search.execute(
                { query: "telemetry fallback", sources: ["note"] },
                toolContext(),
            );

            expect(result).toContain("[1] [note]");
            expect(result).toContain("id=#7 status=dismissed");
            expect(result).toContain("@msg 44");
            expect(result).toContain(NOTE_EXPAND_HINT);
        } finally {
            spy.mockRestore();
        }
    });
});
