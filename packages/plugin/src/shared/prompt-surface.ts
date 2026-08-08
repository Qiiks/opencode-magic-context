/** The built-in prompt-surface variants. */
export type PromptSurfacePreset = "full" | "light";

/**
 * The configuration consumed by prompt-surface resolution. The schema adds
 * validation and defaults; this structural type keeps the resolver usable by
 * every host without importing a config loader.
 */
export interface PromptSurfaceConfig {
    default?: PromptSurfacePreset;
    models?: Readonly<Record<string, PromptSurfacePreset>>;
    guidance_override_path?: string;
    tool_descriptions?: Readonly<Record<string, string>>;
}

export type PromptSurfaceResolutionSource = "exact" | "wildcard" | "default";

/** Validate the literal provider/model and provider/* key grammar. */
export function isValidPromptSurfaceModelKey(key: string): boolean {
    if (key.length === 0 || key.trim() !== key) return false;

    const slash = key.indexOf("/");
    if (slash <= 0 || slash === key.length - 1) return false;

    const provider = key.slice(0, slash);
    const modelID = key.slice(slash + 1);
    if (
        provider.trim() !== provider ||
        modelID.trim() !== modelID ||
        provider.includes("*") ||
        (modelID.includes("*") && modelID !== "*")
    ) {
        return false;
    }
    if (modelID === "*") return true;

    return (
        modelID.length > 0 &&
        !modelID.startsWith("/") &&
        !modelID.endsWith("/") &&
        !modelID.includes("//")
    );
}

export type ModelKeyLookupSource = PromptSurfaceResolutionSource | "bare";

export interface ModelKeyCandidate {
    key: string;
    source: ModelKeyLookupSource;
}

/**
 * Return the same progressive model-key candidates used by cache_ttl. The
 * provider/model boundary is the first slash; the rest of the string remains
 * the model ID, including additional slashes. Candidates are case-sensitive.
 *
 * Provider wildcards are checked after progressively less-specific model keys,
 * but before the caller's default. That keeps an exact or base-model override
 * authoritative while still allowing `provider/*` to cover otherwise-unlisted
 * models.
 */
export function modelKeyLookupOrder(modelKey: string | undefined): ModelKeyCandidate[] {
    if (!modelKey) return [];

    const slash = modelKey.indexOf("/");
    if (slash <= 0 || slash === modelKey.length - 1) return [];

    const provider = modelKey.slice(0, slash);
    let modelID = modelKey.slice(slash + 1);
    const candidates: ModelKeyCandidate[] = [];

    while (modelID.length > 0) {
        candidates.push({ key: `${provider}/${modelID}`, source: "exact" });
        candidates.push({ key: modelID, source: "bare" });

        const lastDash = modelID.lastIndexOf("-");
        if (lastDash <= 0) break;
        modelID = modelID.slice(0, lastDash);
    }

    candidates.push({ key: `${provider}/*`, source: "wildcard" });

    const seen = new Set<string>();
    return candidates.filter((candidate) => {
        if (seen.has(candidate.key)) return false;
        seen.add(candidate.key);
        return true;
    });
}

/** Resolve one per-model value using the shared cache_ttl lookup walk. */
export function resolveModelConfigValue<T>(
    values: Readonly<Record<string, T>> | undefined,
    modelKey: string | undefined,
): { value: T; source: ModelKeyLookupSource } | undefined {
    if (!values) return undefined;

    for (const candidate of modelKeyLookupOrder(modelKey)) {
        const value = values[candidate.key];
        if (value !== undefined) {
            return { value, source: candidate.source };
        }
    }

    return undefined;
}

/**
 * Resolve the prompt preset for a model. Invalid or missing model keys use the
 * configured default and are reported as a default resolution rather than a
 * partial match.
 */
export function resolvePromptSurface(
    config: PromptSurfaceConfig | undefined,
    modelKey: string | undefined,
): { preset: PromptSurfacePreset; source: PromptSurfaceResolutionSource } {
    const fallback = config?.default ?? "full";
    const match = resolveModelConfigValue(config?.models, modelKey);

    // Bare model keys are accepted by cache_ttl for compatibility, but the
    // prompt_surface schema deliberately accepts only provider/model keys and
    // provider wildcards. Ignore a bare entry if an unvalidated object reaches
    // this API so prompt routing keeps its documented grammar.
    if (match && match.source !== "bare") {
        return { preset: match.value, source: match.source };
    }

    return { preset: fallback, source: "default" };
}

/** Resolve a cache_ttl-style value with the shared model-key walk. */
export function resolveModelConfigOrDefault<T>(
    values: Readonly<Record<string, T>>,
    modelKey: string | undefined,
    fallback: T,
): T {
    return resolveModelConfigValue(values, modelKey)?.value ?? fallback;
}
