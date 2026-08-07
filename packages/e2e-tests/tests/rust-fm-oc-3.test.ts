/// <reference types="bun-types" />

/** FM-OC-3: a parked session self-heals when a killed external module returns. */

import { afterEach, beforeEach, describe, expect, it } from "bun:test";
import { RustTestHarness } from "../src/rust-harness";
import {
    assertLoudModuleFailure,
    assertMessagesHaveNoPlaceholders,
    driveToSteadyState,
    RUST_FAILURE_PARK_THRESHOLD,
    RUST_PARK_RETRY_INTERVAL,
    rustPrereqs,
    sendOutagePasses,
} from "../src/rust-scenario-support";

describe.skipIf(!rustPrereqs.ok)("rust failure-mode drill FM-OC-3: parked self-heal", () => {
    let h: RustTestHarness;

    beforeEach(async () => {
        h = await RustTestHarness.create({
            modelContextLimit: 100_000,
            magicContextConfig: { execute_threshold_percentage: 40, protected_tags: 1 },
        });
    });

    afterEach(async () => {
        await h?.dispose();
    });

    it(
        "recovers within the exported retry budget without restarting the session",
        async () => {
            h.subc.assertModuleNotSupervised();
            const sessionId = await h.createSession();
            await driveToSteadyState(h, sessionId, 2);
            const healthyVersions = h
                .readRustPasses()
                .map((pass) => pass.rowVersion)
                .filter((version) => version > 0);
            const beforeCount = h.readRustPasses().length;

            h.subc.killModule();
            await sendOutagePasses(
                h,
                sessionId,
                4,
                RUST_FAILURE_PARK_THRESHOLD,
                "FM-OC-3 outage",
            );

            await h.subc.restoreModule();
            await sendOutagePasses(
                h,
                sessionId,
                4 + RUST_FAILURE_PARK_THRESHOLD,
                RUST_PARK_RETRY_INTERVAL * 2,
                "FM-OC-3 recovery",
            );

            const passes = await h.waitForRustPasses(
                beforeCount + RUST_FAILURE_PARK_THRESHOLD + RUST_PARK_RETRY_INTERVAL * 2,
            );
            const recovery = passes.slice(beforeCount + RUST_FAILURE_PARK_THRESHOLD);
            expect(recovery.length).toBeLessThanOrEqual(RUST_PARK_RETRY_INTERVAL * 2);
            expect(recovery.some((pass) => pass.servedFrom === "transform")).toBe(true);

            const recoveryVersions = recovery
                .map((pass) => pass.rowVersion)
                .filter((version) => version > 0);
            expect(recoveryVersions.length).toBeGreaterThan(0);
            const allVersions = [...healthyVersions, ...recoveryVersions];
            expect(allVersions.every((version, index) => index === 0 || version >= allVersions[index - 1]!)).toBe(
                true,
            );
            expect(recoveryVersions.at(-1)).toBeGreaterThan(healthyVersions.at(-1) ?? 0);
            expect(h.diagnosticLog()).toContain("mc_rust_park_transition");
            assertLoudModuleFailure(h, sessionId);
            assertMessagesHaveNoPlaceholders(h.lastMainMessages(), sessionId);
        },
        300_000,
    );
});
