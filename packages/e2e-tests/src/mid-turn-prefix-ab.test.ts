/// <reference types="bun-types" />

import { describe, expect, it } from "bun:test";
import {
    assertMockAbEvidence,
    buildMidTurnRequest,
    commonPrefixBytes,
    runMockAbExperiment,
    stablePromptBytes,
} from "./mid-turn-prefix-ab";

describe("mid-turn prefix A/B instrument", () => {
    it("measures one suffix rewrite and then reuses the revised prefix", async () => {
        const result = await runMockAbExperiment({ steps: 6, applyAtStep: 3, dropChars: 12_000 });
        const mutation = result.apply[2];
        const holdMutation = result.hold[2];
        const next = result.apply[3];

        expect(result.hold).toHaveLength(6);
        expect(result.apply).toHaveLength(6);
        expect(result.apply[0]).toMatchObject({
            cacheReadBytes: result.hold[0].cacheReadBytes,
            cacheCreationBytes: result.hold[0].cacheCreationBytes,
        });
        expect(result.apply[1]).toMatchObject({
            cacheReadBytes: result.hold[1].cacheReadBytes,
            cacheCreationBytes: result.hold[1].cacheCreationBytes,
        });
        expect(mutation.cacheCreationBytes).toBeGreaterThan(holdMutation.cacheCreationBytes);
        expect(mutation.cacheReadBytes).toBeGreaterThan(0);
        expect(mutation.cacheReadBytes).toBeLessThan(mutation.firstDivergenceByte);
        expect(next.cacheCreationBytes).toBeLessThan(mutation.cacheCreationBytes);
        expect(next.cacheReadBytes).toBe(mutation.stableBytes);
    });

    it("reports the exact byte offset and suffix length for the mutation", () => {
        const options = {
            steps: 6,
            applyAtStep: 3,
            dropChars: 12_000,
            ttl: "5m" as const,
            model: "mock-model",
        };
        const before = stablePromptBytes(buildMidTurnRequest("apply", 2, options, "byte-proof"));
        const after = stablePromptBytes(buildMidTurnRequest("apply", 3, options, "byte-proof"));
        const divergence = commonPrefixBytes(before, after);

        expect(divergence).toBeGreaterThan(0);
        expect(divergence).toBeLessThan(before.byteLength - 8_000);
        expect(after.byteLength - divergence).toBeGreaterThan(1_000);
    });

    it("rejects a vacuous result that claims the rewrite repeats forever", async () => {
        const result = await runMockAbExperiment({ steps: 5, applyAtStep: 3, dropChars: 8_000 });
        result.apply[3].cacheCreationBytes = result.apply[2].cacheCreationBytes;
        expect(() => assertMockAbEvidence(result)).toThrow("revised prefix was not reusable");
    });
});
