import { describe, expect, it } from "bun:test";
import {
  fallbackEntries,
  modelEntryWithModel,
  modelEntryWithQualifier,
  modelId,
  modelQualifier,
} from "./HarnessModelFields";

describe("harness model entries", () => {
  it("keeps free-text provider/model values usable", () => {
    expect(modelEntryWithModel(undefined, "opencode", "private/model")).toBe("private/model");
    expect(modelEntryWithModel(undefined, "pi", "local/model")).toBe("local/model");
  });

  it("writes only the selected harness qualifier", () => {
    const openCodeEntry = modelEntryWithQualifier("openai/gpt-5", "opencode", "high");
    const piEntry = modelEntryWithQualifier("openai/gpt-5", "pi", "high");

    expect(openCodeEntry).toEqual({ model: "openai/gpt-5", variant: "high" });
    expect(piEntry).toEqual({ model: "openai/gpt-5", thinking_level: "high" });
    expect(modelQualifier(openCodeEntry, "pi")).toBeUndefined();
    expect(modelQualifier(piEntry, "opencode")).toBeUndefined();
  });

  it("keeps fallback entries separate and ignores malformed values", () => {
    const entries = fallbackEntries([
      "anthropic/claude-sonnet",
      { model: "openai/gpt-5", variant: "fast" },
      { model: 42 },
      null,
    ]);

    expect(entries).toEqual([
      "anthropic/claude-sonnet",
      { model: "openai/gpt-5", variant: "fast" },
    ]);
    expect(modelId(entries[1])).toBe("openai/gpt-5");
  });
});
