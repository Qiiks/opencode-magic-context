/// <reference types="bun-types" />

/**
 * Incident regression #1: mid-session message removal wedged the ordinal
 * resolver permanently.
 *
 * The bug: when a message is removed mid-session (session.revert, which opencode
 * emits as `message.removed`), the Rust adapter's ordinal resolver saw the raw
 * array shrink under its stored anchor and entered a permanent mismatch loop —
 * every later pass failed to resolve ordinals and the transform stopped serving
 * (parking the session). The fix is a self-heal re-prime that rebuilds the
 * ordinal map from the durable rows after a removal.
 *
 * Status on this branch: verified empirically that a real revert STILL wedges the
 * session permanently here (error → error → error → parked, no recovery) — and it
 * is NOT healed by the merged tail-readopt / park-self-heal fix, because that fix
 * addresses tail identity drift and park pressure, not the mid-array ordinal
 * mismatch a removal creates. In Rust mode tags/index live in the module store
 * (not context.db), so the plugin's clear-and-reindex finds no rows to reconcile
 * and the ordinal memo's stored-count never re-primes.
 *
 * So this regression is gated on its OWN flag (MC_RUST_E2E_REMOVAL=1), separate
 * from the shipped P0 fix, and activates cleanly once the removal
 * ordinal-reconcile self-heal lands. The assertion targets the OUTCOME — after a
 * real removal the transform keeps serving and the session never permanently
 * parks — so it validates the future fix regardless of mechanism.
 *
 * Drives the FULL production path: opencode → plugin → subc daemon → ck-mc.
 */

import { afterEach, beforeEach, describe, expect, it } from "bun:test";
import { RustTestHarness } from "../src/rust-harness";
import {
    driveToSteadyState,
    printSkip,
    REMOVAL_SKIP_REASON,
    removalHealEnabled,
    rustPrereqs,
} from "../src/rust-scenario-support";

const active = rustPrereqs.ok && removalHealEnabled();

describe.skipIf(!rustPrereqs.ok)("rust incident regression: removal self-heal", () => {
    // Visibility: when prereqs are met but the removal-reconcile self-heal is not
    // yet available, print why this regression is dormant instead of silently
    // omitting it.
    it.skipIf(active)("is gated on the removal ordinal-reconcile self-heal", () => {
        printSkip("removal-self-heal", REMOVAL_SKIP_REASON);
        expect(removalHealEnabled()).toBe(false);
    });

    let h: RustTestHarness;

    beforeEach(async () => {
        if (!active) return;
        h = await RustTestHarness.create({
            modelContextLimit: 100_000,
            magicContextConfig: { execute_threshold_percentage: 40, protected_tags: 1 },
        });
    });

    afterEach(async () => {
        await h?.dispose();
    });

    it.skipIf(!active)(
        "keeps transforming after a mid-session message is removed (no permanent park)",
        async () => {
            const sessionId = await h.createSession();
            await driveToSteadyState(h, sessionId, 4);

            const passesBefore = h.readRustPasses();
            expect(passesBefore.some((p) => p.servedFrom === "transform")).toBe(true);
            expect(passesBefore.every((p) => p.decision !== "error")).toBe(true);

            // Pick a MID-session user message to remove (not newest, not first).
            // Reverting to it drops it and everything after — the exact shape
            // session.revert produces and opencode persists as message.removed.
            const messages = await h.listMessages(sessionId);
            const userIds = messages
                .map((m) => m.info)
                .filter((info): info is { id: string; role: string } =>
                    Boolean(info?.id) && info?.role === "user",
                )
                .map((info) => info.id);
            expect(userIds.length).toBeGreaterThanOrEqual(3);
            const midUserId = userIds[Math.floor(userIds.length / 2)]!;

            await h.revertMessage(sessionId, midUserId);
            // Let the async clear-and-reindex settle (the re-prime source).
            await Bun.sleep(2_000);

            const passCountBeforeNext = h.readRustPasses().length;

            // The passes after the removal MUST recover — this is the exact point
            // the old resolver wedged. Drive fresh turns with realistic spacing.
            for (let i = 6; i <= 10; i += 1) {
                h.mock.setDefault({
                    text: `post-removal assistant ${i}`,
                    usage: {
                        input_tokens: 2_000 * i,
                        output_tokens: 20,
                        cache_creation_input_tokens: 1_000,
                    },
                });
                await h.sendPrompt(sessionId, `post-removal turn ${i}: ${h.ballast(400)}`);
                await Bun.sleep(400);
            }

            const allAfter = await h.waitForRustPasses(passCountBeforeNext + 3);
            const passesAfter = allAfter.slice(passCountBeforeNext);

            // Outcome invariants (no permanent wedge):
            //  - the session is not permanently parked (the last pass serves), and
            //  - the module resumed real transforms after the removal, proving the
            //    ordinal resolver re-primed rather than looping forever.
            const lastPass = passesAfter.at(-1)!;
            expect(lastPass.decision).not.toBe("parked");
            expect(passesAfter.some((p) => p.servedFrom === "transform")).toBe(true);
        },
        300_000,
    );
});
