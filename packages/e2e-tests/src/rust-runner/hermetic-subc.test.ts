/// <reference types="bun-types" />

import { describe, expect, it } from "bun:test";
import { __hermeticSubcTest } from "./hermetic-subc";

describe("hermetic Rust process isolation", () => {
    it("reaps only stale PID records", () => {
        const nowMs = 10 * __hermeticSubcTest.stalePidAgeMs;

        expect(
            __hermeticSubcTest.isStaleRustE2ePidRecord(
                nowMs - __hermeticSubcTest.stalePidAgeMs + 1,
                nowMs,
            ),
        ).toBe(false);
        expect(
            __hermeticSubcTest.isStaleRustE2ePidRecord(
                nowMs - __hermeticSubcTest.stalePidAgeMs,
                nowMs,
            ),
        ).toBe(true);
        expect(__hermeticSubcTest.isStaleRustE2ePidRecord(nowMs + 1, nowMs)).toBe(false);
        expect(__hermeticSubcTest.isStaleRustE2ePidRecord(Number.NaN, nowMs)).toBe(false);
    });
});
