export class RawFallbackContextLimitError extends Error {
    readonly code = "RAW_FALLBACK_CONTEXT_LIMIT";
    readonly recoverable = true;

    constructor(
        readonly estimatedTokens: number,
        readonly contextLimitTokens: number,
        options?: { cause?: unknown },
    ) {
        super(
            `Magic Context could not safely serve the raw prompt: the best local estimate is ${estimatedTokens.toLocaleString()} tokens, above the known ${contextLimitTokens.toLocaleString()}-token context limit. Retry the turn so the module can reconnect.`,
            options,
        );
        this.name = "RawFallbackContextLimitError";
    }
}
