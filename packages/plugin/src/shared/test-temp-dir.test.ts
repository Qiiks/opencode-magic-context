import { afterEach, describe, expect, it } from "bun:test";
import { existsSync, mkdirSync, utimesSync } from "node:fs";
import { join } from "node:path";
import {
    cleanupTestTempDir,
    createTestTempDir,
    sweepStaleTestTempDirs,
    withTestTempDir,
} from "./test-temp-dir";

const tempDirs: string[] = [];

afterEach(() => {
    for (const directory of tempDirs.splice(0)) cleanupTestTempDir(directory);
});

function createFixtureRoot(): string {
    const { dir } = createTestTempDir("mc-test-temp-dir-helper-");
    tempDirs.push(dir);
    return dir;
}

describe("test temp directories", () => {
    it("removes a root when its fixture callback throws", () => {
        let directory = "";

        expect(() =>
            withTestTempDir("mc-test-temp-dir-helper-", (tempDir) => {
                directory = tempDir;
                throw new Error("fixture setup failed");
            }),
        ).toThrow("fixture setup failed");

        expect(existsSync(directory)).toBe(false);
    });

    it("sweeps only aged directories with recognized prefixes", () => {
        const root = createFixtureRoot();
        const staleRecognized = join(root, "mc-config-secret-stale");
        const freshRecognized = join(root, "mc-config-secret-fresh");
        const staleBystander = join(root, "unrelated-tool-stale");
        const nowMs = Date.now();

        for (const directory of [staleRecognized, freshRecognized, staleBystander]) {
            mkdirSync(directory);
        }
        utimesSync(staleRecognized, new Date(nowMs - 7_200_000), new Date(nowMs - 7_200_000));
        utimesSync(staleBystander, new Date(nowMs - 7_200_000), new Date(nowMs - 7_200_000));

        const removed = sweepStaleTestTempDirs({ tempDir: root, nowMs });

        expect(removed).toEqual([staleRecognized]);
        expect(existsSync(staleRecognized)).toBe(false);
        expect(existsSync(freshRecognized)).toBe(true);
        expect(existsSync(staleBystander)).toBe(true);
    });
});
