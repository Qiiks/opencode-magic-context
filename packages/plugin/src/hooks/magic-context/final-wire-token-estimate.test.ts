import { afterEach, describe, expect, it } from "bun:test";
import {
    __resetToolDefinitionMeasurements,
    recordToolDefinition,
} from "../../features/magic-context/tool-definition-tokens";
import {
    estimateFinalWireInputTokens,
    type FinalWireTokenEstimate,
} from "./final-wire-token-estimate";
import type { MessageLike } from "./tag-messages";
import { evaluateEmergencyFailClosed } from "./transform-postprocess-phase";

const MODEL = { providerID: "test-provider", modelID: "test-model", agentName: "build" };

afterEach(() => __resetToolDefinitionMeasurements());

function estimate(messages: MessageLike[]): FinalWireTokenEstimate {
    recordToolDefinition(MODEL.providerID, MODEL.modelID, MODEL.agentName, "read", "Read a file", {
        type: "object",
        properties: { path: { type: "string" } },
    });
    return estimateFinalWireInputTokens({
        messages,
        systemPromptTokens: 10_000,
        ...MODEL,
    });
}

function toolMessage(output: string): MessageLike {
    return {
        info: { id: "tool-owner", role: "assistant" },
        parts: [
            {
                type: "tool",
                state: { input: { path: "large.log" }, output },
            },
        ],
    } as unknown as MessageLike;
}

function decide(finalWireInputTokens: number, inputLimit: number) {
    return evaluateEmergencyFailClosed({
        usagePercentage: 108,
        finalWireInputTokens,
        trustedInputLimitTokens: inputLimit,
        emergencyRecoveryArmed: false,
        usagePercentageSynthetic: false,
    });
}

describe("final outgoing-wire token estimate", () => {
    it("sees a flushed pending drop on every repeated aborted-pass retry", () => {
        const largeOutput = Array.from({ length: 40_000 }, (_, index) => `token_${index}`).join(
            " ",
        );
        const message = toolMessage(largeOutput);
        const beforeDrop = estimate([message]);
        (message.parts[0] as { state: { output: string } }).state.output = "[dropped]";
        const afterDrop = estimate([message]);
        const inputLimit = Math.floor((beforeDrop.tokens + afterDrop.tokens) / 2.1);

        expect(beforeDrop.trusted).toBe(true);
        expect(afterDrop.tokens).toBeLessThan(inputLimit);
        expect(beforeDrop.tokens).toBeGreaterThan(inputLimit * 1.05);
        expect(decide(beforeDrop.tokens, inputLimit).shouldAbort).toBe(true);
        expect(decide(afterDrop.tokens, inputLimit).shouldAbort).toBe(false);
        expect(decide(afterDrop.tokens, inputLimit).shouldAbort).toBe(false);
    });

    it("aborts a zero-trim rebuilt fold because the final wire stayed over", () => {
        const unchanged = estimate([
            toolMessage(Array.from({ length: 20_000 }, (_, index) => `fold_${index}`).join(" ")),
        ]);
        const inputLimit = Math.floor(unchanged.tokens / 1.1);

        expect(decide(unchanged.tokens, inputLimit)).toMatchObject({
            shouldAbort: true,
            reason: "numeric-overflow",
        });
    });

    it("allows a completed recomp refresh whose final messages are actually trimmed", () => {
        const trimmed = estimate([
            {
                info: { id: "summary", role: "user" },
                parts: [
                    { type: "text", text: "<session-history>compact summary</session-history>" },
                ],
            } as MessageLike,
        ]);

        expect(decide(trimmed.tokens, trimmed.tokens + 5_000)).toMatchObject({
            shouldAbort: false,
            reason: "numeric-safe",
        });
    });
});
