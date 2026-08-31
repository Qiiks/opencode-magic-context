/// <reference types="bun-types" />

import { describe, expect, it } from "bun:test";
import {
    comparePairedReplayPasses,
    type ReplayFixture,
} from "../src/paired-session-replay";

const fixture: ReplayFixture = {
    schema: 1,
    source: { report: "test", capture: "synthetic", sanitization: "none" },
    passes: [
        {
            label: "one",
            input_text_bytes: 1,
            response: { blocks: [], input_tokens: 1, output_tokens: 1 },
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
});
