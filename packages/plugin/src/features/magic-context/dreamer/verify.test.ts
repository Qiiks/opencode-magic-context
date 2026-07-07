/// <reference types="bun-types" />

import { afterEach, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";

import { Database } from "../../../shared/sqlite";
import { closeQuietly } from "../../../shared/sqlite-helpers";
import {
    getProjectEmbeddings,
    insertMemory,
    loadAllEmbeddings,
    peekProjectEmbeddings,
    resetEmbeddingCacheForTests,
    saveEmbedding,
} from "../memory";
import { getMemoryById } from "../memory/storage-memory";
import {
    getMemoryVerifications,
    recordMemoryVerifications,
} from "../memory/storage-memory-verifications";
import { runMigrations } from "../migrations";
import { initializeDatabase } from "../storage-db";
import { acquireLease } from "./lease";
import { applyVerifyManifest, type VerifyArgs } from "./verify";

const tempDirs: string[] = [];

function freshDb(): Database {
    const db = new Database(":memory:");
    initializeDatabase(db);
    runMigrations(db);
    return db;
}

function tempProject(): string {
    const dir = mkdtempSync(path.join(tmpdir(), "mc-verify-"));
    tempDirs.push(dir);
    mkdirSync(path.join(dir, "src"), { recursive: true });
    writeFileSync(path.join(dir, "src", "old.ts"), "export const oldValue = 1;", "utf8");
    writeFileSync(path.join(dir, "src", "new.ts"), "export const newValue = 2;", "utf8");
    return dir;
}

function verifyArgs(db: Database, sessionDirectory: string, projectIdentity: string): VerifyArgs {
    const holderId = "verify-holder";
    const leaseKey = `verify-${Math.random()}`;
    expect(acquireLease(db, holderId, leaseKey)).toBe(true);
    return {
        db,
        client: {} as never,
        projectIdentity,
        parentSessionId: undefined,
        sessionDirectory,
        holderId,
        leaseKey,
        deadline: Date.now() + 60_000,
    };
}

afterEach(() => {
    resetEmbeddingCacheForTests();
    for (const dir of tempDirs.splice(0)) {
        rmSync(dir, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
    }
});

describe("applyVerifyManifest", () => {
    test("content rewrites clear stale file mappings and embedding cache", async () => {
        const db = freshDb();
        try {
            const projectIdentity = "git:test";
            const dir = tempProject();
            const memory = insertMemory(db, {
                projectPath: projectIdentity,
                category: "ARCHITECTURE",
                content: "Old value lives in src/old.ts.",
                sourceSessionId: "ses",
            });
            recordMemoryVerifications(db, memory.id, ["src/old.ts"], 1_000);
            saveEmbedding(db, memory.id, new Float32Array([1, 2, 3, 4]), "model-a");
            expect(getProjectEmbeddings(db, projectIdentity, "model-a").has(memory.id)).toBe(true);

            const result = await applyVerifyManifest(
                verifyArgs(db, dir, projectIdentity),
                [
                    {
                        id: memory.id,
                        category: memory.category,
                        content: memory.content,
                        mappedFiles: ["src/old.ts"],
                    },
                ],
                `<verify><update id="${memory.id}" files="src/new.ts">New value lives in src/new.ts.</update></verify>`,
            );

            expect(result).toEqual({ verified: 0, updated: 1, archived: 0 });
            expect(getMemoryById(db, memory.id)?.content).toBe("New value lives in src/new.ts.");
            expect(getMemoryVerifications(db, [memory.id]).has(memory.id)).toBe(false);
            expect(loadAllEmbeddings(db, projectIdentity, "model-a").has(memory.id)).toBe(false);
            expect(peekProjectEmbeddings(projectIdentity, "model-a")?.has(memory.id)).toBe(false);
        } finally {
            closeQuietly(db);
        }
    });

    test("rejects conflicting terminal verdicts for the same memory id", async () => {
        const db = freshDb();
        try {
            const projectIdentity = "git:test";
            const dir = tempProject();
            const memory = insertMemory(db, {
                projectPath: projectIdentity,
                category: "ARCHITECTURE",
                content: "Old value lives in src/old.ts.",
                sourceSessionId: "ses",
            });
            recordMemoryVerifications(db, memory.id, ["src/old.ts"], 1_000);

            await expect(
                applyVerifyManifest(
                    verifyArgs(db, dir, projectIdentity),
                    [
                        {
                            id: memory.id,
                            category: memory.category,
                            content: memory.content,
                            mappedFiles: ["src/old.ts"],
                        },
                    ],
                    `<verify><verified id="${memory.id}" files="src/old.ts"/><archive id="${memory.id}" reason="stale"/></verify>`,
                ),
            ).rejects.toThrow(/duplicate id/);

            expect(getMemoryById(db, memory.id)?.status).toBe("active");
            const state = getMemoryVerifications(db, [memory.id]).get(memory.id);
            expect(state?.files).toEqual(["src/old.ts"]);
            expect(state?.verifiedAt).toBe(1_000);
        } finally {
            closeQuietly(db);
        }
    });

    test("rejects a truncated manifest before any DB write", async () => {
        const db = freshDb();
        try {
            const projectIdentity = "git:test";
            const dir = tempProject();
            const memory = insertMemory(db, {
                projectPath: projectIdentity,
                category: "ARCHITECTURE",
                content: "Old value lives in src/old.ts.",
                sourceSessionId: "ses",
            });
            recordMemoryVerifications(db, memory.id, ["src/old.ts"], 1_000);

            await expect(
                applyVerifyManifest(
                    verifyArgs(db, dir, projectIdentity),
                    [
                        {
                            id: memory.id,
                            category: memory.category,
                            content: memory.content,
                            mappedFiles: ["src/old.ts"],
                        },
                    ],
                    `<verify><archive id="${memory.id}" reason="stale"/>`,
                ),
            ).rejects.toThrow(/closing root/);

            expect(getMemoryById(db, memory.id)?.status).toBe("active");
            const state = getMemoryVerifications(db, [memory.id]).get(memory.id);
            expect(state?.files).toEqual(["src/old.ts"]);
            expect(state?.verifiedAt).toBe(1_000);
        } finally {
            closeQuietly(db);
        }
    });
});
