import { afterEach, describe, expect, it } from "bun:test";
import {
    _resetSynapseClientForTests,
    getSynapseLaneIdentity,
    SYNAPSE_MAX_INPUT_TOKENS,
    type SynapseClientLike,
    SynapseEmbeddingProvider,
} from "./embedding-synapse";

class MockSynapseClient implements SynapseClientLike {
    readonly requests: Array<{ method: string; params: unknown }> = [];
    private batchAttempts = 0;
    constructor(private readonly batchSize = 2) {}

    async call<Response = unknown>(
        _module: string,
        method: string,
        params?: unknown,
    ): Promise<Response> {
        this.requests.push({ method, params });
        if (method === "models.list") {
            return {
                models: [
                    {
                        model: "gte-modernbert-base-f16",
                        fingerprint: "fp-live",
                        table_epoch: 0,
                        dims: 3,
                        recommended_batch: this.batchSize,
                        provenance: { source: "fixture" },
                    },
                ],
            } as Response;
        }
        if (method === "embed.query") {
            return {
                vector: [1, 2, 3],
                fingerprint: "fp-live",
                table_epoch: 0,
            } as Response;
        }
        if (method === "embed.batch") {
            this.batchAttempts += 1;
            if (this.batchAttempts === 1) {
                const error = new Error("module is loading") as Error & {
                    code: string;
                    retry_after_ms: number;
                };
                error.code = "model_loading";
                error.retry_after_ms = 0;
                throw error;
            }
            const request = params as { items: Array<{ id: string; content_sha256: string }> };
            return {
                items: request.items.map((item) => ({
                    id: item.id,
                    embedding: [1, 2, 3],
                    content_sha256: item.content_sha256,
                    fingerprint: "fp-live",
                    table_epoch: 0,
                })),
            } as Response;
        }
        throw new Error(`unexpected method ${method}`);
    }

    close(): void {}
}

afterEach(() => {
    _resetSynapseClientForTests();
});

describe("SynapseEmbeddingProvider", () => {
    it("discovers a certified model and sends the required artifact constraints", async () => {
        const client = new MockSynapseClient();
        const provider = new SynapseEmbeddingProvider({
            connectionFile: "fixture",
            projectRoot: "/repo",
            session: "ses-1",
            clientFactory: async () => client,
        });

        expect(await provider.initialize()).toBe(true);
        expect(provider.maxInputTokens).toBe(SYNAPSE_MAX_INPUT_TOKENS);
        expect(provider.modelId).toBe(getSynapseLaneIdentity("gte-modernbert-base-f16", "fp-live"));

        const vector = await provider.embed("hello");
        expect(vector).toEqual(new Float32Array([1, 2, 3]));
        const request = client.requests.find((entry) => entry.method === "embed.query");
        expect(request?.params).toMatchObject({
            model: "gte-modernbert-base-f16",
            required_fingerprint: "fp-live",
            required_epoch: 0,
            allow_equivalent: false,
            accept_declared: false,
        });
    });

    it("honors the live recommended batch size and retries model loading with retry_after_ms", async () => {
        const client = new MockSynapseClient(1);
        const provider = new SynapseEmbeddingProvider({
            connectionFile: "fixture",
            projectRoot: "/repo",
            session: "ses-1",
            clientFactory: async () => client,
        });

        const vectors = await provider.embedItems([
            { id: "memory:1", text: "one", contentSha256: "a" },
            { id: "memory:2", text: "two", contentSha256: "b" },
        ]);

        expect(vectors.size).toBe(2);
        expect(client.requests.filter((entry) => entry.method === "embed.batch")).toHaveLength(3);
        const keys = client.requests
            .filter((entry) => entry.method === "embed.batch")
            .map((entry) => (entry.params as { request_key: string }).request_key);
        expect(keys[0]).toBe(keys[1]);
    });

    it("rejects served fingerprint substitution without adapting", async () => {
        const client = new MockSynapseClient();
        client.call = async <Response = unknown>(
            _module: string,
            method: string,
            params?: unknown,
        ) => {
            client.requests.push({ method, params });
            if (method === "models.list") {
                return {
                    models: [
                        {
                            model: "gte-modernbert-base-f16",
                            fingerprint: "fp-live",
                            table_epoch: 0,
                            dims: 3,
                        },
                    ],
                } as Response;
            }
            return { vector: [1, 2, 3], fingerprint: "fp-other", table_epoch: 0 } as Response;
        };
        const provider = new SynapseEmbeddingProvider({
            connectionFile: "fixture",
            projectRoot: "/repo",
            session: "ses-1",
            clientFactory: async () => client,
        });

        expect(await provider.embed("hello")).toBeNull();
        expect(await provider.embed("again")).toBeNull();
    });
});
