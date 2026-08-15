/// <reference types="bun-types" />

import { afterEach, describe, expect, mock, test } from "bun:test";

import { Database } from "../../../shared/sqlite";
import { closeQuietly } from "../../../shared/sqlite-helpers";
import { evaluateSmartNotes } from "../dreamer/evaluate-smart-notes";
import { acquireLease } from "../dreamer/lease";
import { createDreamTaskExecutor } from "../dreamer/task-executor";
import { leaseKeyFor } from "../dreamer/task-registry";
import { runMigrations } from "../migrations";
import { initializeDatabase } from "../storage-db";
import { addNote, getPendingSmartNotes } from "../storage-notes";
import { runDueCompiledSmartNoteChecks } from "./runner";
import { SMART_NOTE_CHECK_POLICY_VERSION } from "./types";
import { __wakePlaneTest, WAKE_PLANE_CAPABILITY, wakePlaneStatus } from "./wake-plane";

const PROJECT = "git:wake-plane-test";

function freshDb(): Database {
    const db = new Database(":memory:");
    initializeDatabase(db);
    runMigrations(db);
    return db;
}

function catalog(hasWakePlane: boolean) {
    return hasWakePlane
        ? [{ module_id: "scheduled-wakes", roles: [], control_ops: [WAKE_PLANE_CAPABILITY] }]
        : [{ module_id: "other-module", roles: [], control_ops: ["other.operation"] }];
}

function dueCompiledNote(db: Database) {
    const note = addNote(db, "smart", {
        projectPath: PROJECT,
        content: "Check the scheduled wake handoff.",
        surfaceCondition: "When the condition is met",
    });
    db.prepare(
        `UPDATE notes
         SET compiled_check = ?, check_hash = ?, check_cron = ?, check_version = ?,
             check_status = ?, check_next_due_at = ?, policy_version = ?
         WHERE id = ?`,
    ).run(
        "function check() { return { met: false }; }",
        "wake-plane-check",
        "* * * * *",
        1,
        "compiled",
        0,
        SMART_NOTE_CHECK_POLICY_VERSION,
        note.id,
    );
    return note;
}

afterEach(() => {
    __wakePlaneTest.reset();
});

describe("wakePlaneStatus", () => {
    test("recognizes only the affirmative wake.create catalog capability", async () => {
        __wakePlaneTest.setCatalogProbe(async () => catalog(true));
        expect(await wakePlaneStatus()).toBe("present");

        __wakePlaneTest.reset();
        __wakePlaneTest.setCatalogProbe(async () => catalog(false));
        expect(await wakePlaneStatus()).toBe("absent");

        __wakePlaneTest.reset();
        __wakePlaneTest.setCatalogProbe(async () => {
            throw new Error("daemon unavailable");
        });
        expect(await wakePlaneStatus()).toBe("unknown");
    });

    test("re-probes after the TTL instead of retaining a stale catalog answer", async () => {
        let clock = 10_000;
        let hasWakePlane = false;
        let probes = 0;
        __wakePlaneTest.setNow(() => clock);
        __wakePlaneTest.setCatalogProbe(async () => {
            probes += 1;
            return catalog(hasWakePlane);
        });

        expect(await wakePlaneStatus()).toBe("absent");
        hasWakePlane = true;
        expect(await wakePlaneStatus()).toBe("absent");
        expect(probes).toBe(1);

        clock += __wakePlaneTest.ttlMs;
        expect(await wakePlaneStatus()).toBe("present");
        expect(probes).toBe(2);
    });
});

describe("wake-plane smart-note gates", () => {
    test("present skips the dreamer task and leaves its compiled check pending", async () => {
        const db = freshDb();
        try {
            dueCompiledNote(db);
            __wakePlaneTest.setCatalogProbe(async () => catalog(true));
            const client = {
                session: {
                    list: mock(async () => ({ data: [] })),
                    create: mock(async () => ({ data: { id: "must-not-create" } })),
                    prompt: mock(async () => ({})),
                    messages: mock(async () => ({ data: [] })),
                    delete: mock(async () => ({})),
                },
            };
            const executor = createDreamTaskExecutor({
                client: client as never,
                sessionDirectory: process.cwd(),
                openOpenCodeDb: () => null,
            });

            await expect(
                executor(
                    { task: "evaluate-smart-notes", schedule: "* * * * *", timeoutMinutes: 1 },
                    {
                        db,
                        projectIdentity: PROJECT,
                        holderId: "wake-plane-task-holder",
                        leaseKey: leaseKeyFor("evaluate-smart-notes", PROJECT),
                    },
                ),
            ).resolves.toEqual({ status: "completed" });
            expect(getPendingSmartNotes(db, PROJECT)).toHaveLength(1);
            expect(client.session.create).not.toHaveBeenCalled();
            expect(client.session.prompt).not.toHaveBeenCalled();
        } finally {
            closeQuietly(db);
        }
    });

    test("present also blocks the timer sweep before QuickJS, while absent and unknown run it", async () => {
        for (const status of ["present", "absent", "unknown"] as const) {
            const db = freshDb();
            try {
                dueCompiledNote(db);
                __wakePlaneTest.setCatalogProbe(async () => {
                    if (status === "unknown") throw new Error("daemon vanished");
                    return catalog(status === "present");
                });

                const result = await runDueCompiledSmartNoteChecks({
                    db,
                    projectIdentity: PROJECT,
                    projectRoot: process.cwd(),
                    now: 0,
                });
                expect(result.ran).toBe(status === "present" ? 0 : 1);
                __wakePlaneTest.reset();
            } finally {
                closeQuietly(db);
            }
        }
    });

    test("a daemon that vanishes after reporting present resumes standalone evaluation", async () => {
        const db = freshDb();
        try {
            dueCompiledNote(db);
            let clock = 0;
            let daemonAvailable = true;
            __wakePlaneTest.setNow(() => clock);
            __wakePlaneTest.setCatalogProbe(async () => {
                if (!daemonAvailable) throw new Error("daemon vanished");
                return catalog(true);
            });

            expect(
                await runDueCompiledSmartNoteChecks({
                    db,
                    projectIdentity: PROJECT,
                    projectRoot: process.cwd(),
                    now: 0,
                }),
            ).toMatchObject({ ran: 0 });

            daemonAvailable = false;
            clock += __wakePlaneTest.ttlMs;
            expect(
                await runDueCompiledSmartNoteChecks({
                    db,
                    projectIdentity: PROJECT,
                    projectRoot: process.cwd(),
                    now: 0,
                }),
            ).toMatchObject({ ran: 1 });
        } finally {
            closeQuietly(db);
        }
    });

    test("absent and unknown preserve standalone evaluator behavior", async () => {
        for (const status of ["absent", "unknown"] as const) {
            const db = freshDb();
            try {
                dueCompiledNote(db);
                __wakePlaneTest.setCatalogProbe(async () => {
                    if (status === "unknown") throw new Error("daemon unavailable");
                    return catalog(false);
                });
                const leaseKey = `wake-plane-${status}`;
                expect(acquireLease(db, "holder", leaseKey)).toBe(true);

                await expect(
                    evaluateSmartNotes({
                        db,
                        client: {} as never,
                        projectIdentity: PROJECT,
                        parentSessionId: undefined,
                        sessionDirectory: process.cwd(),
                        holderId: "holder",
                        leaseKey,
                        deadline: Date.now() + 60_000,
                    }),
                ).resolves.toMatchObject({ ran: true, pending: 1 });
                __wakePlaneTest.reset();
            } finally {
                closeQuietly(db);
            }
        }
    });
});
