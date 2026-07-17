/// <reference types="bun-types" />

import { afterEach, describe, expect, it } from "bun:test";
import { runMigrations } from "../../features/magic-context/migrations";
import { getCompartments } from "../../features/magic-context/storage";
import { initializeDatabase } from "../../features/magic-context/storage-db";
import { Database } from "../../shared/sqlite";
import { closeQuietly } from "../../shared/sqlite-helpers";
import { mirrorModuleCompartments } from "./module-state-sync";

const databases: Database[] = [];

afterEach(() => {
    for (const db of databases.splice(0)) closeQuietly(db);
});

describe("module compartment mirror-back", () => {
    it("copies rows after the local watermark idempotently", async () => {
        const db = new Database(":memory:");
        databases.push(db);
        initializeDatabase(db);
        runMigrations(db);
        const calls: number[] = [];
        const reader = {
            async getCompartmentsAfter(_sessionId: string, afterSequence: number) {
                calls.push(afterSequence);
                return afterSequence < 2
                    ? {
                          max_sequence: 2,
                          compartments: [
                              {
                                  sequence: 1,
                                  start_message: 1,
                                  end_message: 2,
                                  start_message_id: "m1#0",
                                  end_message_id: "m2#0",
                                  title: "First",
                                  content: "first content",
                                  created_at: 10,
                              },
                              {
                                  sequence: 2,
                                  start_message: 3,
                                  end_message: 4,
                                  start_message_id: "m3#0",
                                  end_message_id: "m4#0",
                                  title: "Second",
                                  content: "second content",
                                  created_at: 20,
                              },
                          ],
                      }
                    : { max_sequence: 2, compartments: [] };
            },
        };

        await mirrorModuleCompartments({ db, sessionId: "ses-mirror", reader });
        await mirrorModuleCompartments({ db, sessionId: "ses-mirror", reader });

        expect(calls).toEqual([-1, 2]);
        expect(getCompartments(db, "ses-mirror").map((row) => row.sequence)).toEqual([1, 2]);
    });
});
