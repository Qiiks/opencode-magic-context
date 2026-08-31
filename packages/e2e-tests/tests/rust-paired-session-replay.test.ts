/// <reference types="bun-types" />

import { describe, expect, it } from "bun:test";
import { runPairedSessionReplay } from "../src/paired-session-replay";
import { rustPrereqs } from "../src/rust-scenario-support";

describe.skipIf(!rustPrereqs.ok)("TS/Rust paired-session wire replay", () => {
    it("keeps the H13 empty, dropped-placeholder, and signed-thinking value spaces aligned", async () => {
        const result = await runPairedSessionReplay({ providerID: "anthropic" });

        expect(result.divergence_count).toBe(3);
        expect(result.unadjudicated_divergence_count).toBe(0);
        expect(result.passes).toHaveLength(4);
        for (const pass of result.passes) {
            expect(pass.empty_content_shapes.classification).toBe("matched_value_space");
            expect(pass.dropped_placeholder_shapes.classification).toBe("matched_value_space");
            if (pass.reasoning_signature_shapes.classification === "divergent_value_space") {
                expect(pass.reasoning_signature_shapes.adjudication?.decision).toBe(
                    "intentional_difference",
                );
            }
        }

        const droppedPass = result.passes.find(
            (pass) => pass.pass === "raw-empty-assistant-text",
        );
        expect(droppedPass?.dropped_placeholder_shapes.shared).toContain(
            "assistant:isolated_dropped_placeholder",
        );
    }, 600_000);

    it("keeps MC-synthetic non-Anthropic sentinels non-empty in both lanes", async () => {
        const result = await runPairedSessionReplay({ providerID: "mock-anthropic" });

        expect(result.unadjudicated_divergence_count).toBe(0);
        expect(result.divergence_count).toBe(0);
        for (const pass of result.passes) {
            expect(pass.empty_content_shapes).toEqual({
                classification: "matched_value_space",
                ts_only: [],
                rust_only: [],
                shared: [],
            });
        }
        const droppedPass = result.passes.find(
            (pass) => pass.pass === "raw-empty-assistant-text",
        );
        expect(droppedPass?.dropped_placeholder_shapes.shared).toContain(
            "assistant:isolated_dropped_placeholder",
        );
    }, 600_000);
});
