import { describe, expect, it } from "bun:test";
import { canonicalModelIdToPi, getEffectiveModelIds, piModelIdToCanonical } from "./model-ids";

describe("shared-config model IDs", () => {
  it("canonicalizes Pi provider prefixes", () => {
    expect(piModelIdToCanonical("openai-codex/gpt-5")).toBe("openai/gpt-5");
    expect(piModelIdToCanonical("google-antigravity/gemini-2.5-pro")).toBe("google/gemini-2.5-pro");
  });

  it("maps canonical provider prefixes back to Pi", () => {
    expect(canonicalModelIdToPi("openai/gpt-5")).toBe("openai-codex/gpt-5");
    expect(canonicalModelIdToPi("google/gemini-2.5-pro")).toBe("google-antigravity/gemini-2.5-pro");
  });

  it("passes through unknown or malformed provider prefixes", () => {
    expect(piModelIdToCanonical("anthropic/claude-sonnet-4")).toBe("anthropic/claude-sonnet-4");
    expect(canonicalModelIdToPi("custom/model")).toBe("custom/model");
    expect(piModelIdToCanonical("gpt-5")).toBe("gpt-5");
  });

  it("unites Pi and OpenCode models as canonical, deduplicated IDs", () => {
    expect(
      getEffectiveModelIds(
        ["openai-codex/gpt-5", "google-antigravity/gemini-2.5-pro", "anthropic/claude-sonnet-4"],
        ["openai/gpt-5", "openai/o3", "anthropic/claude-sonnet-4"],
      ),
    ).toEqual(["anthropic/claude-sonnet-4", "google/gemini-2.5-pro", "openai/gpt-5", "openai/o3"]);
  });
});
