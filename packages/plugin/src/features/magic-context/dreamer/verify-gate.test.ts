/// <reference types="bun-types" />

import { afterEach, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, unlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { Database } from "../../../shared/sqlite";
import { closeQuietly } from "../../../shared/sqlite-helpers";
import {
    __resetVerificationPathsForTests,
    __setVerificationPathsTestHooks,
    insertMemory,
    readGitFileChangeTimesSince,
    recordMemoryMapping,
    recordMemoryVerifications,
} from "../memory";
import { runMigrations } from "../migrations";
import { initializeDatabase } from "../storage-db";
import { partitionVerifyScope } from "./verify-gate";

const PROJECT = "git:test";
const HEAD_SHA = "1111111111111111111111111111111111111111";

function freshDb(): Database {
    const db = new Database(":memory:");
    initializeDatabase(db);
    runMigrations(db);
    return db;
}

function mem(db: Database, projectPath: string, content: string): number {
    const m = insertMemory(db, {
        projectPath,
        category: "ARCHITECTURE",
        content,
        sourceSessionId: "ses",
    });
    if (!m) throw new Error("insertMemory failed");
    return m.id;
}

function gitCommand(args: readonly string[]): string {
    return JSON.stringify([...args]);
}

function makeGitMetadataDirectory(prefix: string): string {
    const dir = mkdtempSync(join(tmpdir(), prefix));
    dirs.push(dir);
    mkdirSync(join(dir, ".git"));
    writeFileSync(join(dir, "a.ts"), "export const a = 1;\n", "utf8");
    writeFileSync(join(dir, "b.ts"), "export const b = 1;\n", "utf8");
    return dir;
}

function installGitScript(responses: Map<string, string | Error>): void {
    __setVerificationPathsTestHooks({
        execFile: async (file, args, options) => {
            if (file !== "git") {
                throw new Error(`Unexpected binary: ${file}`);
            }
            const response = responses.get(gitCommand(args));
            if (response === undefined) {
                throw new Error(
                    `Unexpected git command for ${options.cwd}: ${JSON.stringify([...args])}`,
                );
            }
            if (response instanceof Error) {
                throw response;
            }
            return { stdout: response, stderr: "" };
        },
    });
}

const dirs: string[] = [];

afterEach(() => {
    __resetVerificationPathsForTests();
    for (const d of dirs) rmSync(d, { recursive: true, force: true });
    dirs.length = 0;
});

describe("partitionVerifyScope (per-memory verified_at gate)", () => {
    test("excludes file-independent (sentinel) and unmapped memories", async () => {
        const db = freshDb();
        const dir = makeGitMetadataDirectory("mc-verify-gate-scope-");
        installGitScript(
            new Map([
                [gitCommand(["rev-parse", "--show-toplevel"]), `${dir}\n`],
                [gitCommand(["log", "--since=@1", "--name-only", "--format=%ct"]), ""],
                [gitCommand(["rev-parse", "HEAD"]), `${HEAD_SHA}\n`],
                [gitCommand(["diff", "--name-only", "-z", HEAD_SHA]), ""],
            ]),
        );
        try {
            const mapped = mem(db, PROJECT, "A in a.ts");
            const independent = mem(db, PROJECT, "Anthropic returns 400 on empty content");
            mem(db, PROJECT, "unmapped fact");
            recordMemoryMapping(db, mapped, ["a.ts"], 1);
            recordMemoryMapping(db, independent, [], 1);

            const gate = await partitionVerifyScope({
                db,
                projectIdentity: PROJECT,
                projectDirectory: dir,
                now: 1000,
            });
            expect(gate.inScopeIds).toEqual([mapped]);
        } finally {
            closeQuietly(db);
        }
    });

    test("never-verified mapped memory is always in scope (verified_at=0)", async () => {
        const db = freshDb();
        const dir = makeGitMetadataDirectory("mc-verify-gate-never-");
        installGitScript(
            new Map([
                [gitCommand(["rev-parse", "--show-toplevel"]), `${dir}\n`],
                [gitCommand(["log", "--since=@2", "--name-only", "--format=%ct"]), ""],
                [gitCommand(["rev-parse", "HEAD"]), `${HEAD_SHA}\n`],
                [gitCommand(["diff", "--name-only", "-z", HEAD_SHA]), ""],
            ]),
        );
        try {
            const m = mem(db, PROJECT, "A in a.ts");
            recordMemoryMapping(db, m, ["a.ts"], 1);
            const gate = await partitionVerifyScope({
                db,
                projectIdentity: PROJECT,
                projectDirectory: dir,
                now: 2000,
            });
            expect(gate.inScopeIds).toEqual([m]);
            expect(gate.mode).toBe("incremental");
        } finally {
            closeQuietly(db);
        }
    });

    test("a verified memory whose file is unchanged is SKIPPED", async () => {
        const db = freshDb();
        const dir = makeGitMetadataDirectory("mc-verify-gate-unchanged-");
        installGitScript(
            new Map([
                [gitCommand(["rev-parse", "--show-toplevel"]), `${dir}\n`],
                [gitCommand(["log", "--since=@10", "--name-only", "--format=%ct"]), ""],
                [gitCommand(["rev-parse", "HEAD"]), `${HEAD_SHA}\n`],
                [gitCommand(["diff", "--name-only", "-z", HEAD_SHA]), ""],
            ]),
        );
        try {
            const m = mem(db, PROJECT, "A in a.ts");
            recordMemoryVerifications(db, m, ["a.ts"], 10_000);
            const gate = await partitionVerifyScope({
                db,
                projectIdentity: PROJECT,
                projectDirectory: dir,
                now: 5000,
            });
            expect(gate.inScopeIds).toEqual([]);
            expect(gate.skippedIds).toEqual([m]);
        } finally {
            closeQuietly(db);
        }
    });

    test("a verified memory whose file changed AFTER verification is in scope", async () => {
        const db = freshDb();
        const dir = makeGitMetadataDirectory("mc-verify-gate-changed-");
        installGitScript(
            new Map([
                [gitCommand(["rev-parse", "--show-toplevel"]), `${dir}\n`],
                [gitCommand(["log", "--since=@1", "--name-only", "--format=%ct"]), "2\na.ts\n"],
                [gitCommand(["rev-parse", "HEAD"]), `${HEAD_SHA}\n`],
                [gitCommand(["diff", "--name-only", "-z", HEAD_SHA]), ""],
            ]),
        );
        try {
            const m = mem(db, PROJECT, "A in a.ts");
            recordMemoryVerifications(db, m, ["a.ts"], 1000);
            const gate = await partitionVerifyScope({
                db,
                projectIdentity: PROJECT,
                projectDirectory: dir,
                now: 2000,
            });
            expect(gate.inScopeIds).toEqual([m]);
        } finally {
            closeQuietly(db);
        }
    });

    test("an uncommitted edit keeps the mapped memory in scope", async () => {
        const db = freshDb();
        const dir = makeGitMetadataDirectory("mc-verify-gate-uncommitted-");
        installGitScript(
            new Map([
                [gitCommand(["rev-parse", "--show-toplevel"]), `${dir}\n`],
                [gitCommand(["log", "--since=@10", "--name-only", "--format=%ct"]), ""],
                [gitCommand(["rev-parse", "HEAD"]), `${HEAD_SHA}\n`],
                [gitCommand(["diff", "--name-only", "-z", HEAD_SHA]), "a.ts\0"],
            ]),
        );
        try {
            const m = mem(db, PROJECT, "A in a.ts");
            recordMemoryVerifications(db, m, ["a.ts"], 10_000);
            const gate = await partitionVerifyScope({
                db,
                projectIdentity: PROJECT,
                projectDirectory: dir,
                now: 5000,
            });
            expect(gate.inScopeIds).toEqual([m]);
        } finally {
            closeQuietly(db);
        }
    });

    test("a deleted mapped file keeps the memory in scope", async () => {
        const db = freshDb();
        const dir = makeGitMetadataDirectory("mc-verify-gate-deleted-");
        unlinkSync(join(dir, "a.ts"));
        installGitScript(
            new Map([
                [gitCommand(["rev-parse", "--show-toplevel"]), `${dir}\n`],
                [gitCommand(["log", "--since=@10", "--name-only", "--format=%ct"]), ""],
                [gitCommand(["rev-parse", "HEAD"]), `${HEAD_SHA}\n`],
                [gitCommand(["diff", "--name-only", "-z", HEAD_SHA]), ""],
            ]),
        );
        try {
            const m = mem(db, PROJECT, "A in a.ts");
            recordMemoryVerifications(db, m, ["a.ts"], 10_000);
            const gate = await partitionVerifyScope({
                db,
                projectIdentity: PROJECT,
                projectDirectory: dir,
                now: 5000,
            });
            expect(gate.inScopeIds).toEqual([m]);
        } finally {
            closeQuietly(db);
        }
    });

    test("git-unavailable verification falls back to full mode", async () => {
        const db = freshDb();
        const dir = makeGitMetadataDirectory("mc-verify-gate-full-");
        installGitScript(
            new Map([
                [
                    gitCommand(["rev-parse", "--show-toplevel"]),
                    new Error("git is temporarily unavailable"),
                ],
            ]),
        );
        try {
            const a = mem(db, PROJECT, "A in a.ts");
            const b = mem(db, PROJECT, "B in b.ts");
            recordMemoryVerifications(db, a, ["a.ts"], 10_000);
            recordMemoryVerifications(db, b, ["b.ts"], 10_000);
            const gate = await partitionVerifyScope({
                db,
                projectIdentity: PROJECT,
                projectDirectory: dir,
                now: 5000,
            });
            expect(gate.mode).toBe("full");
            expect(gate.inScopeIds.sort()).toEqual([a, b].sort());
            expect(gate.skippedIds).toEqual([]);
        } finally {
            closeQuietly(db);
        }
    });

    test("reads commit change times using Unix timestamp --since format", async () => {
        const dir = makeGitMetadataDirectory("mc-verify-gate-log-");
        const beforeChange = Date.parse("2026-01-01T12:00:00Z");
        const changeAt = Date.parse("2026-01-02T00:00:00Z");
        const afterChange = Date.parse("2026-01-03T00:00:00Z");
        installGitScript(
            new Map([
                [gitCommand(["rev-parse", "--show-toplevel"]), `${dir}\n`],
                [
                    gitCommand([
                        "log",
                        `--since=@${Math.floor(beforeChange / 1000)}`,
                        "--name-only",
                        "--format=%ct",
                    ]),
                    `${Math.floor(changeAt / 1000)}\na.ts\n`,
                ],
                [
                    gitCommand([
                        "log",
                        `--since=@${Math.floor(afterChange / 1000)}`,
                        "--name-only",
                        "--format=%ct",
                    ]),
                    "",
                ],
            ]),
        );

        const changeTimes = await readGitFileChangeTimesSince(dir, beforeChange);

        expect(changeTimes?.get("a.ts")).toBe(changeAt);

        const laterTimes = await readGitFileChangeTimesSince(dir, afterChange);
        expect(laterTimes?.has("a.ts")).toBe(false);
    });

    test("verify-broad includes every file-mapped memory regardless of change time", async () => {
        const db = freshDb();
        const dir = makeGitMetadataDirectory("mc-verify-gate-broad-");
        try {
            const a = mem(db, PROJECT, "A in a.ts");
            const b = mem(db, PROJECT, "B in b.ts");
            recordMemoryVerifications(db, a, ["a.ts"], Date.now() + 60_000);
            recordMemoryVerifications(db, b, ["b.ts"], Date.now() + 60_000);
            const gate = await partitionVerifyScope({
                db,
                projectIdentity: PROJECT,
                projectDirectory: dir,
                forceBroad: true,
                now: Date.now(),
            });
            expect(gate.mode).toBe("broad");
            expect(gate.inScopeIds.sort()).toEqual([a, b].sort());
        } finally {
            closeQuietly(db);
        }
    });
});
