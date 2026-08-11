/// <reference types="bun-types" />

import { afterEach, describe, expect, it, setDefaultTimeout } from "bun:test";
import { createHash } from "node:crypto";
import {
    closeSync,
    mkdirSync,
    mkdtempSync,
    openSync,
    readdirSync,
    readFileSync,
    readSync,
    rmSync,
    statSync,
    writeFileSync,
    writeSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { runMigrations } from "@magic-context/core/features/magic-context/migrations";
import {
    initializeDatabase,
    inspectRpcServerDiscovery,
    LATEST_SUPPORTED_VERSION,
} from "@magic-context/core/features/magic-context/storage-db";
import { rpcPortFilePath } from "@magic-context/core/shared/rpc-utils";
import { Database } from "@magic-context/core/shared/sqlite";

import type { PromptIO, PromptSpinner, SelectOption } from "../lib/prompts";
import { REPAIR_DB_EXIT, runRepairDb } from "./doctor-repair-db";

setDefaultTimeout(60_000);

const tempDirs: string[] = [];

class MockPrompts implements PromptIO {
    readonly messages: string[] = [];
    private readonly confirmations: boolean[];

    constructor(confirmations: boolean[] = []) {
        this.confirmations = [...confirmations];
    }

    readonly log = {
        info: (message: string) => this.messages.push(`info:${message}`),
        success: (message: string) => this.messages.push(`success:${message}`),
        warn: (message: string) => this.messages.push(`warn:${message}`),
        error: (message: string) => this.messages.push(`error:${message}`),
        message: (message: string) => this.messages.push(`message:${message}`),
        step: (message: string) => this.messages.push(`step:${message}`),
    };

    intro(message: string): void {
        this.messages.push(`intro:${message}`);
    }

    outro(message: string): void {
        this.messages.push(`outro:${message}`);
    }

    note(message: string, title?: string): void {
        this.messages.push(`note:${title ?? ""}:${message}`);
    }

    spinner(): PromptSpinner {
        return {
            start: () => {},
            stop: () => {},
            message: () => {},
        };
    }

    async confirm(message: string): Promise<boolean> {
        this.messages.push(`confirm:${message}`);
        return this.confirmations.shift() ?? false;
    }

    async text(): Promise<string> {
        throw new Error("unexpected text prompt");
    }

    async selectOne(_message: string, _options: SelectOption[]): Promise<string> {
        throw new Error("unexpected select prompt");
    }

    async selectMany(): Promise<string[]> {
        throw new Error("unexpected multiselect prompt");
    }

    async selectAutocomplete(): Promise<string> {
        throw new Error("unexpected autocomplete prompt");
    }
}

function tempStorage(): string {
    const root = mkdtempSync(join(tmpdir(), "mc-repair-db-"));
    tempDirs.push(root);
    return root;
}

function digest(path: string): string {
    return createHash("sha256").update(readFileSync(path)).digest("hex");
}

function seedCurrentDatabase(dbPath: string): void {
    const db = new Database(dbPath);
    initializeDatabase(db);
    runMigrations(db);
    const insertTag = db.prepare(
        "INSERT INTO tags (session_id, type, status, byte_size, tag_number, harness) VALUES (?, 'message', 'active', ?, ?, 'opencode')",
    );
    const insertCompartment = db.prepare(
        `INSERT INTO compartments
            (session_id, sequence, start_message, end_message, title, content, created_at, harness)
         VALUES ('session-main', ?, ?, ?, ?, ?, ?, 'opencode')`,
    );
    const insertMemory = db.prepare(
        `INSERT INTO memories
            (project_path, category, content, normalized_hash, first_seen_at, created_at, updated_at, last_seen_at)
         VALUES ('/project', 'CONSTRAINTS', ?, ?, ?, ?, ?, ?)`,
    );
    const insertNote = db.prepare(
        `INSERT INTO notes
            (type, status, content, session_id, created_at, updated_at, harness)
         VALUES ('session', 'active', ?, 'session-main', ?, ?, 'opencode')`,
    );
    const insertDreamRun = db.prepare(
        `INSERT INTO dream_runs
            (project_path, started_at, finished_at, holder_id, tasks_json)
         VALUES ('/project', ?, ?, 'test-holder', '[]')`,
    );
    db.transaction(() => {
        for (let index = 1; index <= 300; index++) {
            const content = `tag-${index}-${"t".repeat(700)}`;
            insertTag.run("session-main", Buffer.byteLength(content), index);
            db.prepare(
                "INSERT INTO source_contents (tag_id, session_id, content, created_at, harness) VALUES (?, 'session-main', ?, ?, 'opencode')",
            ).run(index, content, index);
        }
        for (let index = 1; index <= 23; index++) {
            insertCompartment.run(
                index,
                index,
                index,
                `compartment-${index}`,
                `knowledge-${index}-${"c".repeat(200)}`,
                index,
            );
        }
        for (let index = 1; index <= 26; index++) {
            insertMemory.run(
                `memory-${index}-${"m".repeat(200)}`,
                `hash-${index}`,
                index,
                index,
                index,
                index,
            );
        }
        for (let index = 1; index <= 4; index++) insertNote.run(`note-${index}`, index, index);
        for (let index = 1; index <= 3; index++) insertDreamRun.run(index, index);
    })();
    db.exec("PRAGMA wal_checkpoint(TRUNCATE)");
    db.exec("PRAGMA journal_mode=DELETE");
    db.close();
}

function corruptLastTagLeaf(dbPath: string): void {
    // Zero exactly one LEAF page of the `tags` b-tree, so most rows survive and `.recover`
    // has something to salvage. Two approaches are unavailable here: `dbstat` is a
    // compile-time SQLite option (SQLITE_ENABLE_DBSTAT_VTAB) that some bun builds omit, and
    // scanning for row text picks a different page depending on how the build packs cells —
    // both make the fixture depend on the toolchain rather than on the code under test.
    // Instead walk the documented on-disk format: sqlite_master gives the table's root page,
    // and the page header's rightmost-child pointer descends to the last leaf.
    const db = new Database(dbPath, { readonly: true });
    const { page_size: pageSize } = db.prepare("PRAGMA page_size").get() as { page_size: number };
    const { rootpage } = db
        .prepare("SELECT rootpage FROM sqlite_master WHERE type = 'table' AND name = 'tags'")
        .get() as { rootpage: number };
    db.close();

    const fd = openSync(dbPath, "r+");
    try {
        const buffer = Buffer.alloc(pageSize);
        let pageno = rootpage;
        // Page 1 carries the 100-byte database header before its b-tree header.
        for (let depth = 0; depth < 32; depth++) {
            readSync(fd, buffer, 0, pageSize, (pageno - 1) * pageSize);
            const headerAt = pageno === 1 ? 100 : 0;
            const pageType = buffer[headerAt];
            if (pageType === 0x0d) break; // leaf table page
            if (pageType !== 0x05)
                throw new Error(`unexpected page type 0x${pageType.toString(16)}`);
            // Interior table page: the rightmost child pointer lives at header offset 8.
            pageno = buffer.readUInt32BE(headerAt + 8);
        }
        if (buffer[pageno === 1 ? 100 : 0] !== 0x0d) throw new Error("no tags leaf page found");
        writeSync(fd, Buffer.alloc(pageSize), 0, pageSize, (pageno - 1) * pageSize);
    } finally {
        closeSync(fd);
    }
}

function rowCount(db: Database, table: string): number {
    return (db.prepare(`SELECT COUNT(*) AS count FROM ${table}`).get() as { count: number }).count;
}

function integrity(dbPath: string): string[] {
    const db = new Database(dbPath, { readonly: true });
    try {
        return (
            db.prepare("PRAGMA integrity_check").all() as Array<{ integrity_check: string }>
        ).map((row) => row.integrity_check);
    } catch (error) {
        return [error instanceof Error ? error.message : String(error)];
    } finally {
        db.close();
    }
}

afterEach(() => {
    for (const path of tempDirs.splice(0)) rmSync(path, { recursive: true, force: true });
});

describe("doctor repair-db", () => {
    it("backs up and salvages readable rows from a genuinely corrupted SQLite page", async () => {
        const storageDir = tempStorage();
        const dbPath = join(storageDir, "context.db");
        seedCurrentDatabase(dbPath);
        corruptLastTagLeaf(dbPath);
        expect(integrity(dbPath)).not.toEqual(["ok"]);
        const corruptDigest = digest(dbPath);
        const prompts = new MockPrompts();

        const code = await runRepairDb({
            dbPath,
            storageDir,
            prompts,
            deps: { now: () => new Date("2026-08-11T12:34:56.789Z") },
        });

        expect(code).toBe(REPAIR_DB_EXIT.salvaged);
        expect(integrity(dbPath)).toEqual(["ok"]);
        const recovered = new Database(dbPath, { readonly: true });
        const recoveredTags = rowCount(recovered, "tags");
        expect(recoveredTags).toBeGreaterThan(0);
        expect(recoveredTags).toBeLessThan(300);
        expect(rowCount(recovered, "compartments")).toBe(23);
        expect(rowCount(recovered, "memories")).toBe(26);
        expect(rowCount(recovered, "notes")).toBe(4);
        expect(rowCount(recovered, "dream_runs")).toBe(3);
        const version = recovered
            .prepare(
                "SELECT MAX(version) AS version FROM schema_migrations WHERE version < 1000000",
            )
            .get() as { version: number };
        recovered.close();
        expect(version.version).toBe(LATEST_SUPPORTED_VERSION);

        const files = readdirSync(storageDir);
        const backup = files.find((name) => name.startsWith("context.db.corrupt-backup-"));
        const original = files.find((name) => name.startsWith("context.db.corrupt-original-"));
        expect(backup).toBeDefined();
        expect(original).toBeDefined();
        expect(digest(join(storageDir, backup as string))).toBe(corruptDigest);
        expect(digest(join(storageDir, original as string))).toBe(corruptDigest);
        const output = prompts.messages.join("\n");
        expect(output).toContain(`Database: ${dbPath}`);
        expect(output).toContain("Attempting SQLite .recover");
        expect(output).toContain("Row counts BEFORE recovery");
        expect(output).toContain(
            `Schema migration: v${LATEST_SUPPORTED_VERSION} → v${LATEST_SUPPORTED_VERSION}`,
        );
        expect(output).toContain("Row counts AFTER recovery");
        expect(output).toContain("Salvage rates");
        for (const table of ["tags", "compartments", "memories", "notes", "dream_runs"]) {
            expect(output).toContain(`${table}=`);
        }
        expect(output).toContain("Backup:");
    });

    it("reports an unsalvageable database distinctly and preserves every source sidecar", async () => {
        const storageDir = tempStorage();
        const dbPath = join(storageDir, "context.db");
        writeFileSync(dbPath, Buffer.alloc(8192, 0x7f));
        writeFileSync(`${dbPath}-wal`, "synthetic corrupt wal");
        writeFileSync(`${dbPath}-shm`, "synthetic corrupt shm");
        const sourceDigests = Object.fromEntries(
            [dbPath, `${dbPath}-wal`, `${dbPath}-shm`].map((path) => [path, digest(path)]),
        );
        const prompts = new MockPrompts([false]);

        const code = await runRepairDb({
            dbPath,
            storageDir,
            prompts,
            deps: { now: () => new Date("2026-08-11T12:35:56.789Z") },
        });

        expect(code).toBe(REPAIR_DB_EXIT.unsalvageable);
        for (const [path, hash] of Object.entries(sourceDigests)) expect(digest(path)).toBe(hash);
        const backups = readdirSync(storageDir).filter((name) =>
            name.startsWith("context.db.corrupt-backup-"),
        );
        expect(backups).toHaveLength(3);
        expect(backups.some((path) => path.endsWith("-wal"))).toBe(true);
        expect(backups.some((path) => path.endsWith("-shm"))).toBe(true);
        const output = prompts.messages.join("\n");
        expect(output).toContain("SQLite salvage was unsuccessful");
        expect(output).toContain("Row counts BEFORE recovery");
        expect(output).toContain("Row counts AFTER recovery");
        expect(output).toContain("Reset declined");
        expect(output).toContain(`Database remains unchanged: ${dbPath}`);
    });

    it("does not offer destructive reset when the .recover shell could not start", async () => {
        const storageDir = tempStorage();
        const dbPath = join(storageDir, "context.db");
        writeFileSync(dbPath, Buffer.alloc(8192, 0x55));
        const originalDigest = digest(dbPath);
        const prompts = new MockPrompts([true]);

        const code = await runRepairDb({
            dbPath,
            storageDir,
            prompts,
            deps: {
                now: () => new Date("2026-08-11T12:36:56.789Z"),
                sqliteExecutable: join(storageDir, "missing-sqlite3"),
            },
        });

        expect(code).toBe(REPAIR_DB_EXIT.failed);
        expect(digest(dbPath)).toBe(originalDigest);
        const output = prompts.messages.join("\n");
        expect(output).toContain("SQLite .recover could not be started");
        expect(output).toContain("Reset was not offered because salvage did not run");
        expect(output).not.toContain("confirm:");
    });

    it("refuses a live RPC holder without changing any file", async () => {
        const storageDir = tempStorage();
        const dbPath = join(storageDir, "context.db");
        writeFileSync(dbPath, "do not touch");
        writeFileSync(`${dbPath}-wal`, "wal do not touch");
        writeFileSync(`${dbPath}-shm`, "shm do not touch");
        const rpcPath = rpcPortFilePath(storageDir, "/project", process.pid, "repair-test");
        mkdirSync(join(rpcPath, ".."), { recursive: true });
        writeFileSync(
            rpcPath,
            JSON.stringify({
                port: 43123,
                pid: process.pid,
                started_at: 0,
                instance_id: "repair-test",
            }),
        );
        const beforeFiles = readdirSync(storageDir, { recursive: true }).map(String).sort();
        const snapshots = [dbPath, `${dbPath}-wal`, `${dbPath}-shm`, rpcPath].map((path) => ({
            path,
            digest: digest(path),
            mtimeMs: statSync(path).mtimeMs,
        }));
        const prompts = new MockPrompts();
        expect(inspectRpcServerDiscovery(storageDir)).toMatchObject({
            state: "live",
            serverPids: [process.pid],
        });

        const code = await runRepairDb({ dbPath, storageDir, prompts });

        expect(code).toBe(REPAIR_DB_EXIT.refused);
        expect(readdirSync(storageDir, { recursive: true }).map(String).sort()).toEqual(
            beforeFiles,
        );
        for (const snapshot of snapshots) {
            expect(digest(snapshot.path)).toBe(snapshot.digest);
            expect(statSync(snapshot.path).mtimeMs).toBe(snapshot.mtimeMs);
        }
        const output = prompts.messages.join("\n");
        expect(output).toContain(`Refusing to repair the live database: ${dbPath}`);
        expect(output).toContain(`OpenCode server (PID ${process.pid})`);
        expect(output).toContain("Backup: not created");
    });
});
