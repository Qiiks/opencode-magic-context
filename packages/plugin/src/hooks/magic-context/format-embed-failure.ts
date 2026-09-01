import type { EmbeddingFailure } from "../../features/magic-context/memory/embedding-failure";

export function formatEmbedFailureSummary(
    embedded: number,
    remaining: number,
    failure?: EmbeddingFailure,
): string {
    const compartment = `compartment${embedded === 1 ? "" : "s"}`;
    if (!failure) {
        return `Embedded ${embedded} ${compartment}; ${remaining} could not be embedded (the provider returned no result). Run /ctx-embed start again to retry them.`;
    }

    const retry = " Run /ctx-embed start again to retry them.";
    switch (failure.class) {
        case "substitution_rejected":
            return `Embedded ${embedded} ${compartment}; ${remaining} rejected: ${failure.reason}. Fix: set embedding.model to the served spelling.`;
        case "http_error": {
            const fix =
                failure.reason.startsWith("HTTP 401") || failure.reason.startsWith("HTTP 403")
                    ? " Fix: check embedding.api_key."
                    : failure.reason.startsWith("HTTP 402")
                      ? " Fix: check provider quota or billing."
                      : failure.reason.startsWith("HTTP 404")
                        ? " Fix: check embedding.endpoint and embedding.model."
                        : failure.retryable
                          ? retry
                          : " Fix: check embedding.endpoint, embedding.model, and credentials.";
            return `Embedded ${embedded} ${compartment}; ${remaining} failed: ${failure.reason}.${fix}`;
        }
        case "invalid_envelope":
            return `Embedded ${embedded} ${compartment}; ${remaining} failed: ${failure.reason}. Fix: configure an OpenAI-compatible embedding endpoint that returns data[].embedding.`;
        case "certification_refusal":
            return `Embedded ${embedded} ${compartment}; ${remaining} rejected: ${failure.reason}. Fix: recertify SYNAPSE or configure embedding.fallback_provider, then run /ctx-embed start again.`;
        case "credential_required":
            return `Embedded ${embedded} ${compartment}; ${remaining} rejected: ${failure.reason}. Fix: reauthenticate SYNAPSE, then run /ctx-embed start again.`;
        case "local_binding_missing":
            return `Embedded ${embedded} ${compartment}; ${remaining} failed: ${failure.reason}. Fix: run \`npx @cortexkit/magic-context@latest doctor\` to verify the WASM fallback. On Intel macOS, onnxruntime-node@1.23.0 may be pinned manually for optional native speed.`;
        case "local_fs_unavailable":
            return `Embedded ${embedded} ${compartment}; ${remaining} failed: ${failure.reason}. Fix: update or reinstall Magic Context so its Node WASM bundle can persist the model, then run /ctx-embed start again.`;
        case "local_download_failure":
            return `Embedded ${embedded} ${compartment}; ${remaining} failed: ${failure.reason}. Fix: check network/proxy access to the model host.${retry}`;
        case "local_runtime_error":
            return `Embedded ${embedded} ${compartment}; ${remaining} failed: ${failure.reason}. Fix: run \`npx @cortexkit/magic-context@latest doctor\` for local runtime diagnostics.`;
        case "empty_result":
        case "transport_error":
            return `Embedded ${embedded} ${compartment}; ${remaining} failed: ${failure.reason}.${failure.retryable ? retry : ""}`;
    }
}
