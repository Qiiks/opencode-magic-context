import { describe, expect, test } from "bun:test";
import type { EmbeddingFailure } from "../../features/magic-context/memory/embedding-failure";
import { formatEmbedFailureSummary } from "./format-embed-failure";

const failures: EmbeddingFailure[] = [
    {
        class: "substitution_rejected",
        reason: "served model 'bge-m3' does not match requested 'baai/bge-m3-embedding' (substitution guard)",
        retryable: false,
    },
    {
        class: "http_error",
        reason: "HTTP 402 from endpoint: quota exhausted",
        retryable: false,
    },
    {
        class: "empty_result",
        reason: "response data[] was empty",
        retryable: true,
    },
    {
        class: "invalid_envelope",
        reason: "response had keys [object, results] but data[] was absent",
        retryable: false,
    },
    {
        class: "certification_refusal",
        reason: "SYNAPSE certification refused embedding: not_certified",
        retryable: false,
    },
    {
        class: "local_binding_missing",
        reason: "onnxruntime-node has no darwin/x64 native binding and the WASM fallback could not complete",
        retryable: false,
    },
    {
        class: "local_fs_unavailable",
        reason: "the WASM model cache cannot access the Node filesystem",
        retryable: false,
    },
    {
        class: "local_download_failure",
        reason: "the embedding model download failed: fetch failed",
        retryable: true,
    },
    {
        class: "local_runtime_error",
        reason: "the local embedding runtime failed: session creation failed",
        retryable: false,
    },
];

describe("formatEmbedFailureSummary", () => {
    test.each(failures)("surfaces $class without a misleading retry instruction", (failure) => {
        const summary = formatEmbedFailureSummary(0, 193, failure);
        expect(summary).toContain(failure.reason);
        if (failure.retryable) {
            expect(summary).toContain("Run /ctx-embed start again to retry them.");
        } else {
            expect(summary).not.toContain("Run /ctx-embed start again to retry them.");
        }
    });

    test("renders Intel native-binding guidance without claiming force reinstall repairs it", () => {
        const summary = formatEmbedFailureSummary(0, 7, {
            class: "local_binding_missing",
            reason: "onnxruntime-node has no darwin/x64 native binding and the WASM fallback could not complete",
            retryable: false,
        });

        expect(summary).toContain("darwin/x64");
        expect(summary).toContain("onnxruntime-node@1.23.0");
        expect(summary).not.toContain("provider returned no result");
        expect(summary).not.toContain("doctor --force");
    });

    test("renders certification refusals with the cause and recovery instead of the generic stall message", () => {
        const summary = formatEmbedFailureSummary(0, 193, {
            class: "certification_refusal",
            reason: "SYNAPSE certification refused embedding: not_certified",
            retryable: false,
        });

        expect(summary).toContain("not_certified");
        expect(summary).toContain("recertify SYNAPSE");
        expect(summary).toContain("embedding.fallback_provider");
        expect(summary).not.toContain("provider returned no result");
    });
});
