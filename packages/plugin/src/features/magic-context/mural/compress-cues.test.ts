/// <reference types="bun-types" />

import { describe, expect, test } from "bun:test";

import { Database } from "../../../shared/sqlite";
import { closeQuietly } from "../../../shared/sqlite-helpers";
import { ensureContextStoreUuid, installAuthorityManagedMarker } from "../context-authority";
import { acquireLease } from "../dreamer/lease";
import { getMemoryById, insertMemory, updateMemoryContent } from "../memory";
import { computeNormalizedHash } from "../memory/normalize-hash";
import { runMigrations } from "../migrations";
import { initializeDatabase } from "../storage-db";
import { applyCues, type CompressCuesArgs, runCompressCues } from "./compress-cues";
import {
    computeCueContentHash,
    getMuralCueState,
    memoryNeedsCue,
    setMuralCue,
} from "./storage-mural-cues";

function assistantMessages(text: string) {
    return [
        {
            info: { role: "assistant", time: { created: Date.now() } },
            parts: [{ type: "text", text }],
        },
    ];
}

function successfulCueClient(onPrompt?: () => void) {
    let manifest = "";
    return {
        session: {
            create: async () => ({ data: { id: "cue-child" } }),
            prompt: async (args: { body?: { parts?: Array<{ text?: string }> } }) => {
                const prompt = args.body?.parts?.[0]?.text ?? "";
                const ids = [...prompt.matchAll(/^\[(\d+)\]/gm)].map((match) => Number(match[1]));
                manifest = `<cues>${ids.map((id) => `<cue id="${id}">anchor ${id}</cue>`).join("")}</cues>`;
                onPrompt?.();
                return {};
            },
            messages: async () => ({ data: assistantMessages(manifest) }),
            delete: async () => ({}),
        },
    };
}

function freshDb(): Database {
    const db = new Database(":memory:");
    initializeDatabase(db);
    runMigrations(db);
    return db;
}

function cueArgs(db: Database, projectIdentity: string): CompressCuesArgs {
    const holderId = "compress-holder";
    const leaseKey = `compress-${Math.random()}`;
    expect(acquireLease(db, holderId, leaseKey)).toBe(true);
    return {
        db,
        client: {} as never,
        projectIdentity,
        parentSessionId: undefined,
        sessionDirectory: process.cwd(),
        holderId,
        leaseKey,
        deadline: Date.now() + 60_000,
    };
}

describe("runCompressCues disposition", () => {
    test("banks a completed chunk and reports the deadline remainder", async () => {
        const db = freshDb();
        try {
            const projectIdentity = "git:cues-deadline";
            for (let index = 0; index < 41; index += 1) {
                insertMemory(db, {
                    projectPath: projectIdentity,
                    category: "ARCHITECTURE",
                    content: `Cue fact ${index}.`,
                    sourceSessionId: "ses",
                });
            }
            const args = cueArgs(db, projectIdentity);
            args.client = successfulCueClient(() => {
                args.deadline = Date.now() - 1;
            }) as never;

            const result = await runCompressCues(args);

            expect(result.compressed).toBe(40);
            expect(result.remaining).toBe(1);
            expect(result.complete).toBe(false);
        } finally {
            closeQuietly(db);
        }
    });

    test("reports complete after fully draining the selected set", async () => {
        const db = freshDb();
        try {
            const projectIdentity = "git:cues-complete";
            insertMemory(db, {
                projectPath: projectIdentity,
                category: "ARCHITECTURE",
                content: "Cue fact.",
                sourceSessionId: "ses",
            });
            const args = cueArgs(db, projectIdentity);
            args.client = successfulCueClient() as never;

            const result = await runCompressCues(args);
            expect(result.compressed).toBe(1);
            expect(result.remaining).toBe(0);
            expect(result.complete).toBe(true);
        } finally {
            closeQuietly(db);
        }
    });

    test("reports a swallowed chunk failure as incomplete", async () => {
        const db = freshDb();
        try {
            const projectIdentity = "git:cues-failure";
            insertMemory(db, {
                projectPath: projectIdentity,
                category: "ARCHITECTURE",
                content: "Cue fact.",
                sourceSessionId: "ses",
            });
            const args = cueArgs(db, projectIdentity);
            args.client = {
                session: {
                    create: async () => {
                        throw new Error("provider unavailable");
                    },
                },
            } as never;

            const result = await runCompressCues(args);
            expect(result.complete).toBe(false);
            expect(result.remaining).toBe(1);
        } finally {
            closeQuietly(db);
        }
    });
});

describe("mural cue storage", () => {
    test("setMuralCue writes cue + hash; getMuralCueState reads them back", () => {
        const db = freshDb();
        try {
            const memory = insertMemory(db, {
                projectPath: "git:p",
                category: "ARCHITECTURE",
                content: "some fact",
                sourceSessionId: "s",
            });
            const hash = computeCueContentHash("some fact");
            setMuralCue(db, memory.projectPath, memory.id, "fact anchor", hash);
            const state = getMuralCueState(db, [memory.id]).get(memory.id);
            expect(state).toEqual({ cue: "fact anchor", hash });
        } finally {
            closeQuietly(db);
        }
    });

    test("writes only cue cache columns for an authority-managed memory", () => {
        const db = freshDb();
        try {
            const memory = insertMemory(db, {
                projectPath: "git:managed-cues",
                category: "ARCHITECTURE",
                content: "authoritative content",
                sourceSessionId: "s",
            });
            const uuid = ensureContextStoreUuid(db);
            installAuthorityManagedMarker(db, memory.projectPath, uuid);

            const hash = computeCueContentHash(memory.content);
            expect(() =>
                setMuralCue(db, memory.projectPath, memory.id, "managed cue", hash),
            ).not.toThrow();
            expect(getMuralCueState(db, [memory.id]).get(memory.id)).toEqual({
                cue: "managed cue",
                hash,
            });

            expect(() =>
                db
                    .prepare("UPDATE memories SET content = ? WHERE id = ?")
                    .run("forbidden edit", memory.id),
            ).toThrow(/managed by the Rust module/i);
            expect(getMemoryById(db, memory.id)?.content).toBe("authoritative content");
        } finally {
            closeQuietly(db);
        }
    });

    test("memoryNeedsCue: NULL cue and stale-hash need compression, current does not", () => {
        expect(memoryNeedsCue(undefined, "x")).toBe(true);
        expect(memoryNeedsCue({ cue: null, hash: null }, "x")).toBe(true);
        expect(memoryNeedsCue({ cue: "c", hash: "stale" }, "x")).toBe(true);
        expect(memoryNeedsCue({ cue: "c", hash: computeCueContentHash("x") }, "x")).toBe(false);
    });

    test("editing a memory's content clears its stored cue", () => {
        const db = freshDb();
        try {
            const memory = insertMemory(db, {
                projectPath: "git:p",
                category: "NAMING",
                content: "old content",
                sourceSessionId: "s",
            });
            setMuralCue(
                db,
                memory.projectPath,
                memory.id,
                "old cue",
                computeCueContentHash("old content"),
            );
            expect(getMuralCueState(db, [memory.id]).get(memory.id)?.cue).toBe("old cue");

            updateMemoryContent(db, memory.id, "new content", computeNormalizedHash("new content"));

            const state = getMuralCueState(db, [memory.id]).get(memory.id);
            expect(state?.cue).toBeNull();
            expect(state?.hash).toBeNull();
        } finally {
            closeQuietly(db);
        }
    });
});

describe("applyCues (per-cue validation, skip-not-reject, hash-race)", () => {
    test("writes valid cues and skips invalid ones without rejecting the chunk", () => {
        const db = freshDb();
        try {
            const good = insertMemory(db, {
                projectPath: "git:p",
                category: "ARCHITECTURE",
                content: "good fact",
                sourceSessionId: "s",
            });
            const bad = insertMemory(db, {
                projectPath: "git:p",
                category: "CONSTRAINTS",
                content: "bad fact",
                sourceSessionId: "s",
            });
            const chunk = [
                { memory: good, contentHash: computeCueContentHash(good.content) },
                { memory: bad, contentHash: computeCueContentHash(bad.content) },
            ];
            // The bad cue is an unbalanced-parens violation → skipped, not fatal.
            const manifest = `<cues><cue id="${good.id}">good anchor</cue><cue id="${bad.id}">oops (unbalanced</cue></cues>`;
            const result = applyCues(cueArgs(db, "git:p"), chunk, manifest);
            expect(result.compressed).toBe(1);
            expect(result.skipped).toBe(1);
            // good got its cue; bad stayed NULL (retried next run).
            expect(getMuralCueState(db, [good.id]).get(good.id)?.cue).toBe("good anchor");
            expect(getMuralCueState(db, [bad.id]).get(bad.id)?.cue ?? null).toBeNull();
        } finally {
            closeQuietly(db);
        }
    });

    test("stores the SELECTION-time content hash so an edit mid-run yields a stale (excluded) cue", () => {
        const db = freshDb();
        try {
            const memory = insertMemory(db, {
                projectPath: "git:p",
                category: "ARCHITECTURE",
                content: "original content",
                sourceSessionId: "s",
            });
            // Candidate captured at selection time (hash of the ORIGINAL content).
            const chunk = [{ memory, contentHash: computeCueContentHash("original content") }];

            // The memory is edited AFTER selection but BEFORE the cue is applied.
            updateMemoryContent(
                db,
                memory.id,
                "edited content",
                computeNormalizedHash("edited content"),
            );

            const manifest = `<cues><cue id="${memory.id}">anchor from original</cue></cues>`;
            applyCues(cueArgs(db, "git:p"), chunk, manifest);

            // The stored hash is the ORIGINAL content's hash, which no longer
            // matches the current ("edited") content — so the cue is stale and
            // memoryNeedsCue re-selects it next run.
            const current = getMemoryById(db, memory.id)!;
            const state = getMuralCueState(db, [memory.id]).get(memory.id)!;
            expect(state.hash).toBe(computeCueContentHash("original content"));
            expect(state.hash).not.toBe(computeCueContentHash(current.content));
            expect(memoryNeedsCue(state, current.content)).toBe(true);
        } finally {
            closeQuietly(db);
        }
    });

    test.each([
        ["missing", (_id: number) => `<cues></cues>`, /missing id/],
        [
            "duplicate",
            (id: number) => `<cues><cue id="${id}">one</cue><cue id="${id}">two</cue></cues>`,
            /duplicate id/,
        ],
        [
            "foreign",
            (id: number) => `<cues><cue id="${id}">ok</cue><cue id="99999">stray</cue></cues>`,
            /unknown id/,
        ],
    ])("rejects %s manifest membership before writing", (_kind, manifestFor, error) => {
        const db = freshDb();
        try {
            const memory = insertMemory(db, {
                projectPath: "git:p",
                category: "NAMING",
                content: "in chunk",
                sourceSessionId: "s",
            });
            const chunk = [{ memory, contentHash: computeCueContentHash(memory.content) }];

            expect(() => applyCues(cueArgs(db, "git:p"), chunk, manifestFor(memory.id))).toThrow(
                error,
            );
            expect(getMuralCueState(db, [memory.id]).get(memory.id)?.cue ?? null).toBeNull();
        } finally {
            closeQuietly(db);
        }
    });
});
