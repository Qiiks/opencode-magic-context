/// <reference types="bun-types" />

import { describe, expect, test } from "bun:test";

import { Database } from "../../../shared/sqlite";
import { closeQuietly } from "../../../shared/sqlite-helpers";
import { getMemoryById, insertMemory } from "../memory";
import { runMigrations } from "../migrations";
import { initializeDatabase } from "../storage-db";
import { applyClassifications, type ClassifyArgs } from "./classify";
import { acquireLease } from "./lease";

function freshDb(): Database {
    const db = new Database(":memory:");
    initializeDatabase(db);
    runMigrations(db);
    return db;
}

function classifyArgs(db: Database, projectIdentity: string): ClassifyArgs {
    const holderId = "classify-holder";
    const leaseKey = `classify-${Math.random()}`;
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

describe("applyClassifications", () => {
    test("complete manifest applies classification fields", () => {
        const db = freshDb();
        try {
            const projectIdentity = "git:test";
            const memory = insertMemory(db, {
                projectPath: projectIdentity,
                category: "ARCHITECTURE",
                content: "Important project fact.",
                sourceSessionId: "ses",
            });

            const result = applyClassifications(
                classifyArgs(db, projectIdentity),
                [memory],
                `<classify><memory id="${memory.id}" importance="85" scope="project" shareable="true"/></classify>`,
            );

            expect(result.classified).toBe(1);
            const after = getMemoryById(db, memory.id);
            expect(after?.importance).toBe(85);
            expect(after?.scope).toBe("project");
            expect(after?.shareable).toBe(1);
        } finally {
            closeQuietly(db);
        }
    });

    test("truncated manifest rejects before stamping classified_at", () => {
        const db = freshDb();
        try {
            const projectIdentity = "git:test";
            const memory = insertMemory(db, {
                projectPath: projectIdentity,
                category: "ARCHITECTURE",
                content: "Important project fact.",
                sourceSessionId: "ses",
            });
            const before = getMemoryById(db, memory.id);
            const beforeRow = db
                .prepare("SELECT classified_at FROM memories WHERE id = ?")
                .get(memory.id) as { classified_at?: number | null } | undefined;

            expect(() =>
                applyClassifications(
                    classifyArgs(db, projectIdentity),
                    [memory],
                    `<classify><memory id="${memory.id}" importance="85"`,
                ),
            ).toThrow(/closing root/);

            const after = getMemoryById(db, memory.id);
            expect(after?.importance).toBe(before?.importance);
            expect(after?.scope).toBe(before?.scope);
            expect(after?.shareable).toBe(before?.shareable);
            const afterRow = db
                .prepare("SELECT classified_at FROM memories WHERE id = ?")
                .get(memory.id) as { classified_at?: number | null } | undefined;
            expect(afterRow?.classified_at).toBe(beforeRow?.classified_at);
        } finally {
            closeQuietly(db);
        }
    });
});
