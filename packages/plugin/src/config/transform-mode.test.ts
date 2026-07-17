import { describe, expect, it } from "bun:test";

import { resolveTransformMode } from "./transform-mode";

describe("resolveTransformMode", () => {
    it("falls back to ts and warns when rust lacks user-level subc", () => {
        expect(
            resolveTransformMode({
                configured: "rust",
                userTierHasSubc: false,
                shadowTransformEnabled: false,
            }),
        ).toEqual({
            mode: "ts",
            warnings: ["rust mode requires user-level subc configuration; running ts."],
        });
    });

    it("keeps rust when trusted user-level subc is present", () => {
        expect(
            resolveTransformMode({
                configured: "rust",
                userTierHasSubc: true,
                shadowTransformEnabled: false,
            }),
        ).toEqual({ mode: "rust", warnings: [] });
    });

    it("keeps ts without warnings when ts is configured", () => {
        expect(
            resolveTransformMode({
                configured: "ts",
                userTierHasSubc: false,
                shadowTransformEnabled: true,
            }),
        ).toEqual({ mode: "ts", warnings: [] });
    });

    it("warns once per project when rust wins over shadow_transform", () => {
        const args = {
            configured: "rust" as const,
            userTierHasSubc: true,
            shadowTransformEnabled: true,
            projectKey: "transform-mode-shadow-project",
        };

        const first = resolveTransformMode(args);
        const second = resolveTransformMode(args);

        expect(first.mode).toBe("rust");
        expect(first.warnings).toEqual([
            'shadow_transform is ignored while transform_mode is "rust" (a session cannot shadow itself); shadow disabled for these sessions.',
        ]);
        expect(second).toEqual({ mode: "rust", warnings: [] });

        const otherProject = resolveTransformMode({
            ...args,
            projectKey: "another-transform-mode-shadow-project",
        });
        expect(otherProject.warnings).toEqual([
            'shadow_transform is ignored while transform_mode is "rust" (a session cannot shadow itself); shadow disabled for these sessions.',
        ]);
    });

    it("warns when TS-only caveman compression is enabled in rust mode", () => {
        const result = resolveTransformMode({
            configured: "rust",
            userTierHasSubc: true,
            shadowTransformEnabled: false,
            cavemanCompressionEnabled: true,
        });

        expect(result.mode).toBe("rust");
        expect(result.warnings).toEqual([
            "caveman_text_compression is TS-only and inert in rust mode.",
        ]);
    });
});
