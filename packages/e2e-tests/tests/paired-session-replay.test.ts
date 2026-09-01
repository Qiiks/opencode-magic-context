/// <reference types="bun-types" />

import { describe, expect, it } from "bun:test";
import {
    comparePairedReplayPasses,
    type ReplayFixture,
} from "../src/paired-session-replay";

const fixture: ReplayFixture = {
    schema: 2,
    source: { report: "test", capture: "synthetic", sanitization: "none" },
    provider_arms: [
        {
            id: "test",
            provider_id: "test",
            provider_api: "@ai-sdk/anthropic",
            model_id: "test",
            wire_family: "anthropic_messages",
            passes: [
                {
                    label: "one",
                    input_text_bytes: 1,
                    response: { blocks: [], input_tokens: 1, output_tokens: 1 },
                },
            ],
        },
    ],
};

describe("paired-session replay differ", () => {
    it("uses the audit differ's divergent_value_space vocabulary", () => {
        const [row] = comparePairedReplayPasses(
            fixture,
            [JSON.stringify([{ role: "assistant", content: [] }])],
            [JSON.stringify([{ role: "assistant", content: [{ type: "text", text: "ok" }] }])],
        );
        expect(row?.empty_content_shapes).toEqual({
            classification: "divergent_value_space",
            ts_only: ["assistant:content=empty_array"],
            rust_only: [],
            shared: [],
        });
    });

    it("reports matched signed-thinking index shapes", () => {
        const wire = JSON.stringify([
            {
                role: "assistant",
                content: [{ type: "thinking", thinking: "x", signature: "s" }],
            },
        ]);
        const [row] = comparePairedReplayPasses(fixture, [wire], [wire]);
        expect(row?.reasoning_signature_shapes).toEqual({
            classification: "matched_value_space",
            ts_only: [],
            rust_only: [],
            shared: ["thinking:index_0:signed"],
        });
    });

    it("classifies OpenAI Responses sentinels, reasoning items, and tool pairs", () => {
        const wire = JSON.stringify([
            {
                type: "reasoning",
                encrypted_content: "opaque",
                summary: [{ type: "summary_text", text: "fixture" }],
            },
            {
                type: "function_call",
                call_id: "call_fixture",
                name: "bash",
                arguments: "{}",
            },
            {
                type: "function_call_output",
                call_id: "call_fixture",
                output: "[dropped]",
            },
        ]);
        const [row] = comparePairedReplayPasses(fixture, [wire], [wire]);

        expect(row?.empty_content_shapes.shared).toEqual([]);
        expect(row?.dropped_placeholder_shapes.shared).toEqual([
            "tool:isolated_dropped_placeholder",
        ]);
        expect(row?.reasoning_signature_shapes.shared).toEqual([
            "reasoning:index_0:signed",
        ]);
        expect(row?.tool_pairing_shapes).toEqual({
            classification: "matched_value_space",
            ts_only: [],
            rust_only: [],
            shared: ["tool_call:paired"],
        });
    });
});
