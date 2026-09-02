import { afterEach, describe, expect, test } from "bun:test";
import { Database } from "../src/shared/sqlite";
import { execFileSync } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { initializeDatabase } from "../src/features/magic-context/storage-db";
import {
    formatRestoreReport,
    RESTORE_REASON,
    restoreCurateArchives,
} from "./restore-curate-archives";

const roots: string[] = [];

afterEach(() => {
    for (const root of roots.splice(0)) rmSync(root, { recursive: true, force: true });
});

function fixture(): { db: Database; path: string; affectedIds: number[]; legacyId: number } {
    const root = mkdtempSync(join(tmpdir(), "restore-curate-archives-"));
    roots.push(root);
    const path = join(root, "context.db");
    const db = new Database(path);
    // The production schema, not a hand-rolled subset: the memories table carries
    // AFTER UPDATE triggers (FTS maintenance, authority mirror) and bun:sqlite
    // folds trigger-fired writes into `changes`, so a one-row UPDATE reads well
    // above 1. A trigger-free fixture let an equality check on `changes` pass here
    // while skipping every row against the live database.
    initializeDatabase(db);
    const triggerCount = count(
        db,
        "SELECT COUNT(*) AS count FROM sqlite_master WHERE type = 'trigger' AND tbl_name = 'memories'",
    );
    if (triggerCount === 0) throw new Error("fixture must carry the production memories triggers");
    let seq = 0;
    const insertStatement = db.prepare(`
        INSERT INTO memories
            (project_path, category, content, normalized_hash, importance, status, superseded_by_memory_id, metadata_json,
             first_seen_at, created_at, updated_at, last_seen_at)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, 1, 1, 1, 1)
    `);
    const insert = {
        run: (
            project: string,
            category: string,
            content: string,
            importance: number,
            status: string,
            successor: number | null,
            metadata: string | null,
        ) =>
            insertStatement.run(
                project,
                category,
                content,
                `hash-${seq++}`,
                importance,
                status,
                successor,
                metadata,
            ),
    };
    const reason =
        "Redundant with global user profile directives (U2, U3) or superseded by canonical memory entries.";
    const affectedIds = [
        Number(
            insert.run(
                "project-a",
                "PROJECT_RULES",
                "Always run the release check.",
                75,
                "archived",
                null,
                JSON.stringify({ archive_reason: reason, keep: "yes" }),
            ).lastInsertRowid,
        ),
        Number(
            insert.run(
                "project-a",
                "CONFIG_VALUES",
                "release_channel=stable",
                83,
                "archived",
                null,
                JSON.stringify({ archive_reason: "Duplicate of user preferences U9." }),
            ).lastInsertRowid,
        ),
        Number(
            insert.run(
                "project-a",
                "ARCHITECTURE",
                "The registry owns startup.",
                50,
                "archived",
                null,
                JSON.stringify({ archive_reason: "Redundant with the user profile." }),
            ).lastInsertRowid,
        ),
        Number(
            insert.run(
                "project-b",
                "CONSTRAINTS",
                "The provider caps payloads.",
                86,
                "archived",
                null,
                JSON.stringify({ archive_reason: "Overlaps global user-profile preference U4." }),
            ).lastInsertRowid,
        ),
    ];
    const legacyId = Number(
        insert.run(
            "project-a",
            "USER_PREFERENCES",
            "Prefer concise responses.",
            90,
            "archived",
            null,
            JSON.stringify({ archive_reason: reason }),
        ).lastInsertRowid,
    );
    insert.run(
        "project-a",
        "ARCHITECTURE",
        "An unrelated archived row.",
        90,
        "archived",
        null,
        JSON.stringify({ archive_reason: "Stale implementation detail." }),
    );
    insert.run(
        "project-a",
        "PROJECT_RULES",
        "An already active row.",
        90,
        "active",
        null,
        JSON.stringify({ archive_reason: reason }),
    );
    const successorId = Number(
        insert.run(
            "project-a",
            "NAMING",
            "Canonical naming rule.",
            80,
            "active",
            null,
            null,
        ).lastInsertRowid,
    );
    insert.run(
        "project-a",
        "NAMING",
        "Old naming rule.",
        80,
        "archived",
        successorId,
        JSON.stringify({ archive_reason: reason }),
    );
    db.prepare(
        "INSERT INTO project_state (project_path, project_memory_epoch, project_user_profile_version, updated_at) VALUES (?, ?, 0, 1) ON CONFLICT(project_path) DO UPDATE SET project_memory_epoch = excluded.project_memory_epoch",
    ).run("project-a", 4);
    return { db, path, affectedIds, legacyId };
}

function count(db: Database, sql: string): number {
    return (db.prepare(sql).get() as { count: number }).count;
}

function getMemoryStatus(db: Database, id: number): string | undefined {
    return (db.prepare("SELECT status FROM memories WHERE id = ?").get(id) as
        | { status: string }
        | undefined)?.status;
}

describe("restore-curate-archives", () => {
    test("CLI defaults to dry-run", () => {
        const { db, path } = fixture();
        db.close();

        const output = execFileSync(
            process.execPath,
            [join(import.meta.dir, "restore-curate-archives.ts"), "--db", path],
            { encoding: "utf8" },
        );
        const check = new Database(path, { readonly: true });
        try {
            expect(output).toContain("mode: dry-run");
            expect(output).toContain("total: 4");
            expect(count(check, "SELECT COUNT(*) AS count FROM memory_mutation_log")).toBe(0);
            expect(count(check, "SELECT COUNT(*) AS count FROM memories WHERE status = 'active'")).toBe(
                2,
            );
        } finally {
            check.close();
        }
    });

    test("dry-run reports canonical-category casualties and excludes legacy categories", () => {
        const { db, affectedIds, legacyId } = fixture();
        try {
            const before = db.serialize();
            const report = restoreCurateArchives(db, { apply: false, now: 1_788_336_000_000 });
            const output = formatRestoreReport(report);

            expect(report.total).toBe(4);
            expect(report.highImportanceIds).toEqual([
                affectedIds[0],
                affectedIds[1],
                affectedIds[3],
            ]);
            expect(report.highImportanceIds).not.toContain(legacyId);
            expect(report.groups.some((group) => group.category === "USER_PREFERENCES")).toBe(false);
            expect(report.groups).toEqual([
                {
                    project: "project-a",
                    category: "ARCHITECTURE",
                    importanceBand: "50-69",
                    count: 1,
                },
                {
                    project: "project-a",
                    category: "CONFIG_VALUES",
                    importanceBand: "70-84",
                    count: 1,
                },
                {
                    project: "project-a",
                    category: "PROJECT_RULES",
                    importanceBand: "70-84",
                    count: 1,
                },
                {
                    project: "project-b",
                    category: "CONSTRAINTS",
                    importanceBand: "85-100",
                    count: 1,
                },
            ]);
            expect(output).toContain("mode: dry-run");
            expect(output).toContain("project-a | ARCHITECTURE | 50-69 | 1");
            expect(output).toContain(`importance>=70 ids: ${affectedIds[0]}, ${affectedIds[1]}, ${affectedIds[3]}`);
            expect(db.serialize()).toEqual(before);
            expect(count(db, "SELECT COUNT(*) AS count FROM memory_mutation_log")).toBe(0);
        } finally {
            db.close();
        }
    });

    test("apply restores rows, logs updates, bumps each project epoch once, and is idempotent", () => {
        const { db, affectedIds, legacyId } = fixture();
        try {
            const first = restoreCurateArchives(db, { apply: true, now: 1_788_336_000_000 });
            expect(first.total).toBe(4);
            expect(first.restored).toBe(4);

            const rows = db
                .prepare(
                    `SELECT id, status, superseded_by_memory_id, metadata_json
                     FROM memories WHERE id IN (${affectedIds.map(() => "?").join(",")})
                     ORDER BY id`,
                )
                .all(...affectedIds) as Array<{
                id: number;
                status: string;
                superseded_by_memory_id: number | null;
                metadata_json: string;
            }>;
            expect(rows.every((row) => row.status === "active")).toBe(true);
            expect(getMemoryStatus(db, legacyId)).toBe("archived");
            expect(rows.every((row) => row.superseded_by_memory_id === null)).toBe(true);
            expect(
                rows.every((row) => {
                    const metadata = JSON.parse(row.metadata_json) as Record<string, unknown>;
                    return metadata.archive_reason === undefined && metadata.restore_reason === RESTORE_REASON;
                }),
            ).toBe(true);
            expect(JSON.parse(rows[0].metadata_json)).toMatchObject({ keep: "yes" });
            expect(
                count(
                    db,
                    "SELECT COUNT(*) AS count FROM memory_mutation_log WHERE mutation_type = 'update'",
                ),
            ).toBe(4);
            expect(
                db
                    .prepare(
                        "SELECT project_path, project_memory_epoch FROM project_state ORDER BY project_path",
                    )
                    .all(),
            ).toEqual([
                { project_path: "project-a", project_memory_epoch: 5 },
                { project_path: "project-b", project_memory_epoch: 1 },
            ]);

            const second = restoreCurateArchives(db, { apply: true, now: 1_788_336_100_000 });
            expect(second.total).toBe(0);
            expect(second.restored).toBe(0);
            expect(count(db, "SELECT COUNT(*) AS count FROM memory_mutation_log")).toBe(4);
            expect(
                db
                    .prepare(
                        "SELECT SUM(project_memory_epoch) AS count FROM project_state",
                    )
                    .get(),
            ).toEqual({ count: 6 });
        } finally {
            db.close();
        }
    });
});
