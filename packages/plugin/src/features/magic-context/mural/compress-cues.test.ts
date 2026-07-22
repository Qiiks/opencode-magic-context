/// <reference types="bun-types" />

import { describe, expect, test } from "bun:test";

import { Database } from "../../../shared/sqlite";
import { closeQuietly } from "../../../shared/sqlite-helpers";
import { acquireLease } from "../dreamer/lease";
import { getMemoryById, insertMemory, updateMemoryContent } from "../memory";
import { computeNormalizedHash } from "../memory/normalize-hash";
import { runMigrations } from "../migrations";
import { initializeDatabase } from "../storage-db";
import { applyCues, type CompressCuesArgs } from "./compress-cues";
import {
    computeCueContentHash,
    getMuralCueState,
    memoryNeedsCue,
    setMuralCue,
} from "./storage-mural-cues";

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
            setMuralCue(db, memory.id, "fact anchor", hash);
            const state = getMuralCueState(db, [memory.id]).get(memory.id);
            expect(state).toEqual({ cue: "fact anchor", hash });
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
            setMuralCue(db, memory.id, "old cue", computeCueContentHash("old content"));
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

    test("ignores a cue for a memory outside the chunk", () => {
        const db = freshDb();
        try {
            const memory = insertMemory(db, {
                projectPath: "git:p",
                category: "NAMING",
                content: "in chunk",
                sourceSessionId: "s",
            });
            const chunk = [{ memory, contentHash: computeCueContentHash(memory.content) }];
            const manifest = `<cues><cue id="${memory.id}">ok</cue><cue id="99999">stray</cue></cues>`;
            const result = applyCues(cueArgs(db, "git:p"), chunk, manifest);
            expect(result.compressed).toBe(1);
        } finally {
            closeQuietly(db);
        }
    });
});
