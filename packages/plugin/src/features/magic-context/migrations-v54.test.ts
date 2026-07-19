import { afterEach, describe, expect, it } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { Database as DatabaseType } from "../../shared/sqlite";
import { Database, withPrivilegedWriter } from "../../shared/sqlite";
import {
    applyMirrorPage,
    ensureContextStoreUuid,
    getMirrorCursor,
    installAuthorityManagedMarker,
} from "./context-authority";
import { runMigrations } from "./migrations";
import { initializeDatabase } from "./storage-db";

const databases: DatabaseType[] = [];
const tempDirs: string[] = [];

function freshDatabase(path = ":memory:"): DatabaseType {
    const db = new Database(path);
    databases.push(db);
    initializeDatabase(db);
    runMigrations(db);
    return db;
}

afterEach(() => {
    for (const db of databases.splice(0)) db.close();
    for (const dir of tempDirs.splice(0)) rmSync(dir, { recursive: true, force: true });
});

describe("authority-managed context.db schema", () => {
    it("mints a stable store UUID and closes managed memory and smart-note writes", () => {
        const db = freshDatabase();
        const uuid = ensureContextStoreUuid(db);
        expect(uuid).toMatch(/^[0-9a-f-]{36}$/);
        expect(ensureContextStoreUuid(db)).toBe(uuid);
        installAuthorityManagedMarker(db, "/project", uuid);

        expect(() =>
            db
                .prepare(
                    "INSERT INTO memories (project_path, category, content, normalized_hash, first_seen_at, created_at, updated_at, last_seen_at) VALUES (?, ?, ?, ?, 0, 0, 0, 0)",
                )
                .run("/project", "CONSTRAINTS", "blocked", "hash"),
        ).toThrow();
        expect(() =>
            db
                .prepare(
                    "INSERT INTO notes (type, status, content, project_path, created_at, updated_at) VALUES ('smart', 'active', 'blocked', ?, 0, 0)",
                )
                .run("/project"),
        ).toThrow();

        withPrivilegedWriter(db, () => {
            db.prepare(
                "INSERT INTO authority_repair_pending(project_path, started_at) VALUES (?, 0)",
            ).run("/repairing");
        });
        expect(() =>
            db
                .prepare(
                    "INSERT INTO memories (project_path, category, content, normalized_hash, first_seen_at, created_at, updated_at, last_seen_at) VALUES (?, ?, ?, ?, 0, 0, 0, 0)",
                )
                .run("/repairing", "CONSTRAINTS", "blocked", "hash"),
        ).toThrow();
        withPrivilegedWriter(db, () => {
            db.prepare("DELETE FROM authority_repair_pending WHERE project_path = ?").run(
                "/repairing",
            );
        });

        withPrivilegedWriter(db, () => {
            db.prepare(
                "INSERT INTO memories (project_path, category, content, normalized_hash, first_seen_at, created_at, updated_at, last_seen_at) VALUES (?, ?, ?, ?, 0, 0, 0, 0)",
            ).run("/project", "CONSTRAINTS", "allowed", "hash");
            db.prepare(
                "INSERT INTO notes (type, status, content, project_path, created_at, updated_at) VALUES ('smart', 'active', 'allowed', ?, 0, 0)",
            ).run("/project");
        });

        // Session notes are deliberately outside the managed project-owned set.
        db.prepare(
            "INSERT INTO notes (type, status, content, session_id, created_at, updated_at) VALUES ('session', 'active', 'session-owned', ?, 0, 0)",
        ).run("session");
    });

    it("rejects a raw connection that never registered the privilege UDF", () => {
        const dir = mkdtempSync(join(tmpdir(), "mc-authority-"));
        tempDirs.push(dir);
        const path = join(dir, "context.db");
        const managed = freshDatabase(path);
        const uuid = ensureContextStoreUuid(managed);
        installAuthorityManagedMarker(managed, "/project", uuid);
        managed.close();
        databases.splice(databases.indexOf(managed), 1);

        const raw = new Database(path);
        databases.push(raw);
        expect(() =>
            raw
                .prepare(
                    "INSERT INTO memories (project_path, category, content, normalized_hash, first_seen_at, created_at, updated_at, last_seen_at) VALUES (?, ?, ?, ?, 0, 0, 0, 0)",
                )
                .run("/project", "CONSTRAINTS", "raw", "raw"),
        ).toThrow(/function|mc_privileged_writer|managed by the Rust module/i);
    });
});

describe("module changefeed mirror", () => {
    it("keeps identities stable and removes stale vectors in the apply transaction", () => {
        const db = freshDatabase();
        applyMirrorPage({
            db,
            page: {
                domain: "memories",
                cursor: 0,
                next_cursor: 1,
                has_more: true,
                rows: [
                    {
                        feed_seq: 1,
                        domain: "memories",
                        op: "insert",
                        module_row_id: 41,
                        content_hash: "hash-a",
                        full_row_snapshot: {
                            id: 41,
                            project_path: "/project",
                            category: "CONSTRAINTS",
                            content: "one",
                            normalized_hash: "hash-a",
                            status: "active",
                            scope: "project",
                            shareable: 0,
                            created_at: 1,
                            updated_at: 1,
                            first_seen_at: 1,
                            last_seen_at: 1,
                        },
                    },
                ],
            },
        });
        const identity = db
            .prepare(
                "SELECT context_row_id FROM mirror_identity WHERE domain = 'memories' AND module_project = ? AND module_row_id = ?",
            )
            .get("/project", 41) as { context_row_id: number };
        db.prepare(
            "INSERT INTO memory_embeddings(memory_id, embedding, model_id) VALUES (?, ?, ?)",
        ).run(identity.context_row_id, Buffer.from([1]), "model");

        applyMirrorPage({
            db,
            page: {
                domain: "memories",
                cursor: 1,
                next_cursor: 2,
                has_more: false,
                rows: [
                    {
                        feed_seq: 2,
                        domain: "memories",
                        op: "update",
                        module_row_id: 41,
                        content_hash: "hash-b",
                        full_row_snapshot: {
                            id: 41,
                            project_path: "/project",
                            category: "CONSTRAINTS",
                            content: "two",
                            normalized_hash: "hash-b",
                            status: "active",
                            scope: "project",
                            shareable: 0,
                            created_at: 1,
                            updated_at: 2,
                            first_seen_at: 1,
                            last_seen_at: 2,
                        },
                    },
                ],
            },
        });
        expect(getMirrorCursor(db, "memories")).toBe(2);
        expect(
            db.prepare("SELECT COUNT(*) AS count FROM memory_embeddings").get() as {
                count: number;
            },
        ).toEqual({ count: 0 });
        expect(
            db
                .prepare("SELECT id, content FROM memories WHERE id = ?")
                .get(identity.context_row_id) as { id: number; content: string },
        ).toEqual({ id: identity.context_row_id, content: "two" });
    });
});
