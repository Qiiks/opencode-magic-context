/// <reference types="bun-types" />

import { describe, expect, it } from "bun:test";
import {
    runPairedSessionReplay,
    runPairedTrailingBlankSequenceReplay,
} from "../src/paired-session-replay";
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
            expect(pass.tool_pairing_shapes.classification).toBe("matched_value_space");
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

    it("freezes the same tool-ending assistant across the trailing-blank ingress race", async () => {
        const result = await runPairedTrailingBlankSequenceReplay();

        expect(result.ts_stable).toBe(true);
        expect(result.rust_stable).toBe(true);
        expect(result.parity).toBe(true);
        expect(result.stages.late_blank.ts.sha256).toBe(
            result.stages.next_turn_pass_1.ts.sha256,
        );
        expect(result.stages.late_blank.rust.sha256).toBe(
            result.stages.next_turn_pass_1.rust.sha256,
        );
        expect(result.stages.late_blank.ts.decision).toBe("strip");
        expect(result.stages.late_blank.rust.decision).toBe("strip");
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

    it("keeps the OpenAI Responses empty-tool, pairing, and reasoning surfaces aligned", async () => {
        const result = await runPairedSessionReplay({ providerArm: "openai-responses" });

        expect(result.wire_family).toBe("openai_responses");
        expect(result.unadjudicated_divergence_count).toBe(0);
        expect(result.divergence_count).toBe(0);
        expect(result.passes).toHaveLength(2);
        for (const pass of result.passes) {
            expect(pass.empty_content_shapes.classification).toBe("matched_value_space");
            expect(pass.dropped_placeholder_shapes.classification).toBe("matched_value_space");
            expect(pass.reasoning_signature_shapes.classification).toBe("matched_value_space");
            expect(pass.tool_pairing_shapes.classification).toBe("matched_value_space");
        }

        const toolPass = result.passes.find(
            (pass) => pass.pass === "observe-empty-tool-output-with-reasoning",
        );
        expect(toolPass?.empty_content_shapes.shared).toEqual([]);
        expect(toolPass?.dropped_placeholder_shapes.shared).toContain(
            "tool:isolated_dropped_placeholder",
        );
        expect(toolPass?.tool_pairing_shapes.shared).toContain("tool_call:paired");
        const historyPass = result.passes.find(
            (pass) => pass.pass === "observe-responses-history",
        );
        expect(historyPass?.reasoning_signature_shapes.shared).toContain(
            "reasoning:nonzero_index:signed",
        );
    }, 600_000);
});
