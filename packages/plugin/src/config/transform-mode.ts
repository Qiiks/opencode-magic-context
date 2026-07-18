export type ResolvedTransformMode = "ts" | "rust";

export interface ResolveTransformModeArgs {
    configured: ResolvedTransformMode;
    userTierHasSubc: boolean;
    shadowTransformEnabled: boolean;
    /** Stable project identifier used to deduplicate the shadow warning per project. */
    projectKey?: string;
    /** Whether the TS-only caveman compression setting is enabled. */
    cavemanCompressionEnabled?: boolean;
}

const warnedShadowProjects = new Set<string>();
const warnedCavemanProjects = new Set<string>();

const RUST_REQUIRES_USER_SUBC_WARNING =
    "rust mode requires user-level subc configuration; running ts.";
const SHADOW_TRANSFORM_WARNING =
    'shadow_transform is ignored while transform_mode is "rust" (a session cannot shadow itself); shadow disabled for these sessions.';
const CAVEMAN_RUST_WARNING = "caveman_text_compression is TS-only and inert in rust mode.";

export function resolveTransformMode(args: ResolveTransformModeArgs): {
    mode: ResolvedTransformMode;
    warnings: string[];
} {
    if (args.configured === "rust" && !args.userTierHasSubc) {
        return {
            mode: "ts",
            warnings: [RUST_REQUIRES_USER_SUBC_WARNING],
        };
    }

    const warnings: string[] = [];
    if (args.configured === "rust" && args.shadowTransformEnabled) {
        const projectKey = args.projectKey ?? "<unspecified-project>";
        if (!warnedShadowProjects.has(projectKey)) {
            warnedShadowProjects.add(projectKey);
            warnings.push(SHADOW_TRANSFORM_WARNING);
        }
    }

    if (args.configured === "rust" && args.cavemanCompressionEnabled) {
        const projectKey = args.projectKey ?? "<unspecified-project>";
        if (!warnedCavemanProjects.has(projectKey)) {
            warnedCavemanProjects.add(projectKey);
            warnings.push(CAVEMAN_RUST_WARNING);
        }
    }

    return { mode: args.configured, warnings };
}
