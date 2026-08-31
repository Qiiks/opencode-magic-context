/// <reference types="bun-types" />

import { Database } from "bun:sqlite";
import { afterAll, beforeAll, describe, expect, it } from "bun:test";
import { join } from "node:path";
import { RustTestHarness } from "../src/rust-harness";
import { rustPrereqs } from "../src/rust-scenario-support";

function valueOf(response: Record<string, unknown>): Record<string, unknown> {
    return response.result && typeof response.result === "object"
        ? (response.result as Record<string, unknown>)
        : response;
}

async function waitForCompartment(h: RustTestHarness, sessionId: string): Promise<Record<string, unknown>> {
    const deadline = Date.now() + 90_000;
    while (Date.now() < deadline) {
        const status = valueOf(
            await h.subc.moduleStatus(sessionId, h.env.workdir, "session.status"),
        );
        if (Number(status.compartment_count ?? 0) > 0) return status;
        await Bun.sleep(250);
    }
    throw new Error("timed out waiting for a durable Rust compartment");
}

describe.skipIf(!rustPrereqs.ok)("rust maintenance command contract", () => {
    let h: RustTestHarness;

    beforeAll(async () => {
        h = await RustTestHarness.create({
            modelContextLimit: 30_000,
            magicContextConfig: {
                execute_threshold_percentage: 25,
                protected_tags: 1,
                compressor: { enabled: false },
            },
        });
    });

    afterAll(async () => {
        await h?.dispose();
    });

    it("records wrapup and recomp dispositions with monotonic cache versions", async () => {
        const sessionId = await h.createSession();
        for (let turn = 1; turn <= 10; turn += 1) {
            h.mock.setDefault({
                text: `maintenance reply ${turn}`,
                usage: {
                    input_tokens: turn * 3_000,
                    output_tokens: 20,
                    cache_creation_input_tokens: 2_000,
                },
            });
            await h.sendPrompt(
                sessionId,
                `maintenance turn ${turn}: ${h.ballast(2_500)}`,
            );
            await Bun.sleep(150);
        }

        const before = await waitForCompartment(h, sessionId);
        expect(Number(before.coverage_ordinal ?? 0)).toBeGreaterThan(0);

        const store = new Database(
            join(h.env.dataDir, "cortexkit", "magic-context", "store.db"),
            { readonly: true },
        );
        store.exec("PRAGMA query_only = ON");
        try {
            const rowBefore = store
                .prepare("SELECT row_version FROM mc_cache_state WHERE session_id = ?")
                .get(sessionId) as { row_version: number };

            const wrapup = valueOf(
                await h.subc.moduleRequest(sessionId, h.env.workdir, {
                    method: "session.wrapup",
                    keep: 2,
                    command_id: "hunt14-wrapup",
                }),
            );
            if (wrapup.ok !== true) {
                throw new Error(`session.wrapup failed: ${JSON.stringify(wrapup)}`);
            }
            const wrapupDisposition = wrapup.disposition;
            if (typeof wrapupDisposition !== "string") {
                throw new Error("session.wrapup response omitted its disposition");
            }
            expect(wrapupDisposition).toBe("nothing_to_compact");

            const wrapupRow = store
                .prepare(
                    "SELECT disposition, rounds FROM mc_wrapup_commands WHERE session_id = ? AND command_id = ?",
                )
                .get(sessionId, "hunt14-wrapup") as { disposition: string; rounds: number };
            expect(wrapupRow.disposition).toBe(wrapupDisposition);
            expect(wrapupRow.rounds).toBe(0);

            const afterWrapup = valueOf(
                await h.subc.moduleStatus(sessionId, h.env.workdir, "session.status"),
            );
            const rowAfterWrapup = store
                .prepare("SELECT row_version FROM mc_cache_state WHERE session_id = ?")
                .get(sessionId) as { row_version: number };
            expect(rowAfterWrapup.row_version).toBeGreaterThanOrEqual(rowBefore.row_version);
            expect(Number(afterWrapup.coverage_ordinal ?? 0)).toBeGreaterThan(0);

            const recomp = valueOf(
                await h.subc.moduleRequest(sessionId, h.env.workdir, {
                    method: "session.recomp",
                    command_id: "hunt14-recomp",
                }),
            );
            expect(recomp.ok).toBe(true);
            expect(recomp.disposition).toBe("started");

            const recompRow = store
                .prepare(
                    "SELECT disposition FROM mc_recomp_commands WHERE session_id = ? AND command_id = ?",
                )
                .get(sessionId, "hunt14-recomp") as { disposition: string };
            expect(recompRow.disposition).toBe("started");

            const afterRecomp = valueOf(
                await h.subc.moduleStatus(sessionId, h.env.workdir, "session.status"),
            );
            const rowAfterRecomp = store
                .prepare("SELECT row_version FROM mc_cache_state WHERE session_id = ?")
                .get(sessionId) as { row_version: number };
            expect(rowAfterRecomp.row_version).toBeGreaterThan(rowAfterWrapup.row_version);
            expect(Number(afterRecomp.compartment_count ?? 0)).toBe(0);
            expect(afterRecomp.coverage_ordinal ?? null).toBeNull();
        } finally {
            store.close();
        }
    }, 300_000);
});
