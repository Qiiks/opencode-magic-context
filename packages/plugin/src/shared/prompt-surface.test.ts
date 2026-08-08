import { describe, expect, it } from "bun:test";

import { resolveCacheTtl } from "../hooks/magic-context/event-resolvers";
import { modelKeyLookupOrder, resolvePromptSurface } from "./prompt-surface";

describe("prompt-surface resolution", () => {
    it("keeps cache_ttl and prompt-surface routing on the same lookup walk", () => {
        const routes = {
            "anthropic/claude/sonnet": "light" as const,
            "openai/gpt-4o": "light" as const,
            "google/*": "light" as const,
            "CaseSensitive/model": "light" as const,
            "progressive/base": "light" as const,
        };
        const cacheTtl = {
            default: "full",
            ...Object.fromEntries(Object.entries(routes).map(([key, preset]) => [key, preset])),
        };

        const cases = [
            ["anthropic/claude/sonnet", "light"],
            ["anthropic/claude/other", "full"],
            ["openai/gpt-4o", "light"],
            ["openai/gpt-4o-mini", "light"],
            ["google/gemini-pro", "light"],
            ["casesensitive/model", "full"],
            ["CaseSensitive/model", "light"],
            ["progressive/base-extra", "light"],
            ["unknown/model", "full"],
            [undefined, "full"],
        ] as const;

        for (const [modelKey, expected] of cases) {
            const ttl = resolveCacheTtl(cacheTtl, modelKey);
            const prompt = resolvePromptSurface({ default: "full", models: routes }, modelKey);

            expect(ttl).toBe(expected);
            expect(prompt.preset).toBe(expected);
        }
    });

    it("applies exact, wildcard, then default precedence", () => {
        const config = {
            default: "full" as const,
            models: {
                "provider/model": "light" as const,
                "provider/*": "full" as const,
            },
        };

        expect(resolvePromptSurface(config, "provider/model")).toEqual({
            preset: "light",
            source: "exact",
        });
        expect(resolvePromptSurface(config, "provider/other")).toEqual({
            preset: "full",
            source: "wildcard",
        });
        expect(resolvePromptSurface(config, "other/model")).toEqual({
            preset: "full",
            source: "default",
        });
    });

    it("preserves multi-slash model IDs and treats case differences literally", () => {
        expect(modelKeyLookupOrder("provider/model/with/slashes")[0]).toEqual({
            key: "provider/model/with/slashes",
            source: "exact",
        });
        expect(
            resolvePromptSurface(
                {
                    default: "full",
                    models: { "provider/model/with/slashes": "light" },
                },
                "provider/model/with/slashes",
            ),
        ).toEqual({ preset: "light", source: "exact" });
        expect(
            resolvePromptSurface(
                {
                    default: "full",
                    models: { "Provider/*": "light" },
                },
                "provider/model/with/slashes",
            ),
        ).toEqual({ preset: "full", source: "default" });
    });

    it("falls back when provider or model components are absent", () => {
        const config = {
            default: "light" as const,
            models: { "provider/*": "full" as const },
        };

        expect(resolvePromptSurface(config, "provider")).toEqual({
            preset: "light",
            source: "default",
        });
        expect(resolvePromptSurface(config, "/model")).toEqual({
            preset: "light",
            source: "default",
        });
        expect(resolvePromptSurface(config, "provider/")).toEqual({
            preset: "light",
            source: "default",
        });
    });
});
