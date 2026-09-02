import { describe, expect, test } from "bun:test";
import { variantChangeBustsProviderCache } from "./sentinel";

describe("variantChangeBustsProviderCache", () => {
    test.each([
        ["anthropic", "claude-fable-5-1", false],
        ["anthropic", "claude-opus-4-1", true],
        ["anthropic", "claude-fable-5", true],
        ["bedrock", "anthropic.claude-fable-5-1-v1:0", false],
        ["google-vertex-anthropic", "claude-fable-5.1", false],
        ["openai", "claude-fable-5-1", false],
        [undefined, "claude-fable-5-1", false],
    ] as const)("provider=%s model=%s returns %s", (providerID, modelID, expected) => {
        expect(variantChangeBustsProviderCache(providerID, modelID)).toBe(expected);
    });
});
