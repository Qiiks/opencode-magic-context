import { existsSync, readFileSync, statSync } from "node:fs";
import { dirname, isAbsolute, resolve } from "node:path";

import {
    type ConfigHarness,
    cortexKitUserConfigBasePath,
    resolveLegacyConfigSourcesForHarness,
} from "../config/migrate-config-location";
import { detectConfigFile } from "./jsonc-parser";
import {
    type PromptSurfaceConfig,
    type PromptSurfacePreset,
    resolvePromptSurface,
} from "./prompt-surface";

const GUIDANCE_MARKER = "## Magic Context";
const GUIDANCE_MARKER_LINE = /^## Magic Context[\t ]*\r?$/gm;

export const PROMPT_SURFACE_TOOL_IDS = [
    "ctx_reduce",
    "ctx_expand",
    "ctx_note",
    "ctx_memory",
    "ctx_search",
] as const;

export type PromptSurfaceToolId = (typeof PROMPT_SURFACE_TOOL_IDS)[number];

/**
 * Preserve the existing full-preset hash exactly while giving future presets a
 * distinct cache identity, even when a placeholder currently renders the same bytes.
 */
export function promptSurfaceHashMaterial(
    systemContent: string,
    preset: PromptSurfacePreset = "full",
): string {
    return preset === "full"
        ? systemContent
        : `${systemContent}\n\0magic-context-prompt-surface:${preset}`;
}

const PROMPT_SURFACE_TOOL_ID_SET = new Set<string>(PROMPT_SURFACE_TOOL_IDS);

export interface PromptSurfaceGuidanceSelection {
    /** The configured preset. The full renderer currently handles light as a temporary fallback. */
    preset: PromptSurfacePreset;
    /** Complete user-authored primary section captured when a model-key epoch starts. */
    primaryOverride?: string;
}

export interface PromptSurfaceRegistrationSelection {
    preset: PromptSurfacePreset;
    descriptionFor: (toolId: string, fullDescription: string) => string;
}

export interface PromptSurfaceRuntime {
    resolveRegistration: (
        config: PromptSurfaceConfig | undefined,
    ) => PromptSurfaceRegistrationSelection;
    resolveGuidance: (
        config: PromptSurfaceConfig | undefined,
        modelKey: string | undefined,
    ) => PromptSurfaceGuidanceSelection;
}

export interface CreatePromptSurfaceRuntimeOptions {
    harness?: ConfigHarness;
    directory?: string;
    /** Explicit test/integration seam; production derives the USER config directory. */
    userConfigDirectory?: string;
    warn: (message: string) => void;
}

function resolveUserConfigDirectory(options: CreatePromptSurfaceRuntimeOptions): string {
    if (options.userConfigDirectory) return resolve(options.userConfigDirectory);

    const sharedBase = cortexKitUserConfigBasePath();
    const shared = detectConfigFile(sharedBase);
    if (shared.format !== "none") return dirname(shared.path);

    if (options.harness) {
        const legacy = resolveLegacyConfigSourcesForHarness(
            options.directory ?? process.cwd(),
            options.harness,
        ).user.find((source) => existsSync(source.path));
        if (legacy) return dirname(legacy.path);
    }

    return dirname(sharedBase);
}

function markerCount(content: string): number {
    return content.match(GUIDANCE_MARKER_LINE)?.length ?? 0;
}

/**
 * Create one host-registration runtime. Its warning set is shared by tool
 * registration and every guidance epoch, so the temporary light fallback and a
 * bad override file are reported once instead of on every model call.
 */
export function createPromptSurfaceRuntime(
    options: CreatePromptSurfaceRuntimeOptions,
): PromptSurfaceRuntime {
    const userConfigDirectory = resolveUserConfigDirectory(options);
    const warned = new Set<string>();

    const warnOnce = (key: string, message: string): void => {
        if (warned.has(key)) return;
        warned.add(key);
        options.warn(message);
    };

    const noticeMissingLightAssets = (): void => {
        warnOnce(
            "light-assets-unavailable",
            "prompt_surface selected light, but built-in light assets are not available yet; using the byte-identical full guidance and tool descriptions until light assets ship.",
        );
    };

    const readGuidanceOverride = (configuredPath: string | undefined): string | undefined => {
        if (!configuredPath) return undefined;
        const path = isAbsolute(configuredPath)
            ? configuredPath
            : resolve(userConfigDirectory, configuredPath);

        let content: string;
        try {
            if (!statSync(path).isFile()) {
                warnOnce(
                    `guidance-not-file:${path}`,
                    `prompt_surface.guidance_override_path (${path}) is not a file; using built-in guidance.`,
                );
                return undefined;
            }
            content = readFileSync(path, "utf8");
        } catch (error) {
            const reason = error instanceof Error ? error.message : String(error);
            warnOnce(
                `guidance-unreadable:${path}:${reason}`,
                `prompt_surface.guidance_override_path (${path}) could not be read (${reason}); using built-in guidance.`,
            );
            return undefined;
        }

        if (content.trim().length === 0) {
            warnOnce(
                `guidance-empty:${path}`,
                `prompt_surface.guidance_override_path (${path}) is empty; using built-in guidance.`,
            );
            return undefined;
        }

        const markers = markerCount(content);
        if (markers !== 1) {
            warnOnce(
                `guidance-marker-count:${path}:${markers}`,
                `prompt_surface.guidance_override_path (${path}) must contain exactly one ${JSON.stringify(GUIDANCE_MARKER)} section marker; found ${markers}. Using built-in guidance.`,
            );
            return undefined;
        }

        return content;
    };

    return {
        resolveRegistration(config) {
            // OpenCode and Pi expose one immutable provider tool map per host
            // registration. Model routes are intentionally ignored here: only the
            // registration owner's default and user-tier overrides can choose text.
            const { preset } = resolvePromptSurface(config, undefined);
            if (preset === "light") noticeMissingLightAssets();

            const overrides = config?.tool_descriptions ?? {};
            for (const [toolId, description] of Object.entries(overrides)) {
                if (!PROMPT_SURFACE_TOOL_ID_SET.has(toolId)) {
                    warnOnce(
                        `unknown-tool:${toolId}`,
                        `prompt_surface.tool_descriptions.${toolId} is not a known ctx_* tool ID; the override was ignored.`,
                    );
                } else if (description.trim().length === 0) {
                    warnOnce(
                        `empty-tool:${toolId}`,
                        `prompt_surface.tool_descriptions.${toolId} is empty; the override was ignored.`,
                    );
                }
            }

            return {
                preset,
                descriptionFor(toolId, fullDescription) {
                    if (!PROMPT_SURFACE_TOOL_ID_SET.has(toolId)) return fullDescription;
                    const override = overrides[toolId];
                    if (override !== undefined && override.trim().length > 0) return override;
                    // A future light-preset catalog will supply descriptions here.
                    // Until then both presets deliberately materialize full bytes.
                    return fullDescription;
                },
            };
        },

        resolveGuidance(config, modelKey) {
            const { preset } = resolvePromptSurface(config, modelKey);
            if (preset === "light") noticeMissingLightAssets();
            return {
                preset,
                primaryOverride: readGuidanceOverride(config?.guidance_override_path),
            };
        },
    };
}

interface GuidanceEpoch {
    config: PromptSurfaceConfig | undefined;
    modelKey: string | undefined;
    selection: PromptSurfaceGuidanceSelection;
}

/** Freeze preset selection and materialized override bytes for one model-key epoch. */
export function createPromptSurfaceGuidanceEpochCache(runtime: PromptSurfaceRuntime): {
    resolve: (
        sessionId: string,
        config: PromptSurfaceConfig | undefined,
        modelKey: string | undefined,
    ) => PromptSurfaceGuidanceSelection;
    clear: (sessionId: string) => void;
} {
    const epochs = new Map<string, GuidanceEpoch>();

    return {
        resolve(sessionId, config, modelKey) {
            const cached = epochs.get(sessionId);
            if (cached && cached.config === config && cached.modelKey === modelKey) {
                return cached.selection;
            }

            const selection = runtime.resolveGuidance(config, modelKey);
            epochs.set(sessionId, { config, modelKey, selection });
            return selection;
        },
        clear(sessionId) {
            epochs.delete(sessionId);
        },
    };
}
