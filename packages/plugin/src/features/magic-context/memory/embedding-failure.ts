export type EmbeddingFailureClass =
    | "substitution_rejected"
    | "http_error"
    | "transport_error"
    | "invalid_envelope"
    | "empty_result"
    | "certification_refusal"
    | "credential_required"
    | "local_binding_missing"
    | "local_fs_unavailable"
    | "local_download_failure"
    | "local_runtime_error";

/** A classified provider failure that can be surfaced to a caller safely. */
export interface EmbeddingFailure {
    class: EmbeddingFailureClass;
    /** Stable, user-facing diagnostic with only redacted response evidence. */
    reason: string;
    /** Whether the same configuration may plausibly succeed on another attempt. */
    retryable: boolean;
}

export interface LocalEmbeddingFailureContext {
    platform?: NodeJS.Platform;
    arch?: string;
    usesWasm?: boolean;
    nativeError?: unknown;
}

function errorMessage(error: unknown): string {
    return error instanceof Error ? error.message : String(error ?? "unknown error");
}

function nestedMessages(error: unknown): string[] {
    const messages = [errorMessage(error)];
    if (error && typeof error === "object" && "cause" in error) {
        const cause = (error as { cause?: unknown }).cause;
        if (cause !== undefined) messages.push(...nestedMessages(cause));
    }
    return messages;
}

function safeErrorDetail(message: string): string {
    return message
        .replace(/https?:\/\/[^\s"']+/gi, "<remote URL>")
        .replace(/\/(?:Users|home)\/[^/\s]+/g, "~")
        .replace(/\s+/g, " ")
        .trim()
        .slice(0, 240);
}

function hasAny(messages: readonly string[], patterns: readonly string[]): boolean {
    return messages.some((message) => {
        const lower = message.toLowerCase();
        return patterns.some((pattern) => lower.includes(pattern));
    });
}

/** Classify local-provider errors before callers receive an uninformative null result. */
export function classifyLocalEmbeddingFailure(
    error: unknown,
    context: LocalEmbeddingFailureContext = {},
): EmbeddingFailure {
    const primaryMessages = nestedMessages(error);
    const nativeMessages =
        context.nativeError === undefined ? [] : nestedMessages(context.nativeError);

    if (
        hasAny(primaryMessages, [
            "mc_embedding_fs_unavailable",
            "file system cache is not available",
        ]) ||
        (context.usesWasm && hasAny(primaryMessages, ["unable to get model file path or buffer"]))
    ) {
        return {
            class: "local_fs_unavailable",
            reason: "the WASM model cache cannot access the Node filesystem",
            retryable: false,
        };
    }

    if (
        hasAny(primaryMessages, [
            "failed to fetch",
            "fetch failed",
            "network error",
            "enotfound",
            "econnreset",
            "econnrefused",
            "certificate",
            "unable to load file",
            "unauthorized access to file",
            "forbidden access to file",
            "not found at",
        ]) ||
        primaryMessages.some((message) => /\bHTTP\s+[45]\d\d\b/i.test(message))
    ) {
        return {
            class: "local_download_failure",
            reason: `the embedding model download failed: ${safeErrorDetail(primaryMessages[0])}`,
            retryable: true,
        };
    }

    const bindingMessages = [...primaryMessages, ...nativeMessages];
    if (
        hasAny(bindingMessages, [
            "onnxruntime_binding.node",
            "cannot find package 'onnxruntime-node'",
            'cannot find package "onnxruntime-node"',
            "cannot find module 'onnxruntime-node'",
            'cannot find module "onnxruntime-node"',
            "could not resolve: onnxruntime-node",
        ])
    ) {
        const platform = context.platform ?? process.platform;
        const arch = context.arch ?? process.arch;
        return {
            class: "local_binding_missing",
            reason:
                platform === "darwin" && arch === "x64"
                    ? "onnxruntime-node has no darwin/x64 native binding and the WASM fallback could not complete"
                    : `onnxruntime-node has no usable native binding for ${platform}/${arch} and the WASM fallback could not complete`,
            retryable: false,
        };
    }

    return {
        class: "local_runtime_error",
        reason: `the local embedding runtime failed: ${safeErrorDetail(primaryMessages[0])}`,
        retryable: false,
    };
}

export function dominantEmbeddingFailure(
    failures: readonly EmbeddingFailure[],
): EmbeddingFailure | undefined {
    const counts = new Map<string, { failure: EmbeddingFailure; count: number }>();
    for (const failure of failures) {
        const key = `${failure.class}\u0000${failure.reason}`;
        const current = counts.get(key);
        if (current) current.count += 1;
        else counts.set(key, { failure, count: 1 });
    }

    let dominant: { failure: EmbeddingFailure; count: number } | undefined;
    for (const candidate of counts.values()) {
        if (!dominant || candidate.count > dominant.count) dominant = candidate;
    }
    return dominant?.failure;
}
