/// <reference types="bun-types" />

import { describe, expect, it } from "bun:test";
import {
    hasShareabilitySensitiveText,
    isSecretKey,
    sanitizeConfigValue,
    sanitizeDiagnosticEndpoint,
} from "./redaction";

describe("isSecretKey — true positives", () => {
    // Real secret-bearing key names from common providers and config shapes.
    const SHOULD_REDACT = [
        "api_key",
        "apiKey",
        "API_KEY",
        "x-api-key",
        "x_api_key",
        "openai_api_key",
        "OPENAI_API_KEY",
        "anthropic_api_key",
        "azure_api_key",
        "huggingface_api_key",
        "aws_secret_access_key",
        "aws_access_key_id",
        "secret_key",
        "secretKey",
        "access_token",
        "accessToken",
        "refresh_token",
        "refreshToken",
        "private_key",
        "client_secret",
        "auth_token",
        "session_token",
        "bearer_token",
        "github_token",
        "huggingface_token",
        "service_credential",
        "password",
        "Password",
        "PASSWORD",
        "credential",
        // Bare segment alone.
        "token",
        "key",
        "secret",
        "auth",
        "bearer",
    ];

    for (const key of SHOULD_REDACT) {
        it(`redacts ${key}`, () => {
            expect(isSecretKey(key)).toBe(true);
        });
    }
});

describe("isSecretKey — false positives we deliberately reject", () => {
    // Real config field names from packages/plugin/src/config/schema/magic-context.ts
    // that DO contain a secret word as a substring but are NOT secrets.
    // The substring-based pattern shipped through v0.21.2 wrongly redacted
    // ALL of these in `doctor --issue` and dashboard config dumps,
    // confusing reporters and hiding values that should be visible.
    const SHOULD_NOT_REDACT = [
        // Magic Context config fields that triggered the regression on issue #85.
        "pin_key_files",
        "token_budget",
        "execute_threshold_tokens",
        "injection_budget_tokens",
        "key_files", // a sub-field of pin_key_files
        // Generic plain English compounds that aren't secrets.
        "key_value", // map-style label
        "tokens_per_second",
        "auth_method", // enum value name, not credential — `method` isn't a descriptor
        "session_count", // session is qualifier but `count` isn't a secret word
        "tokens_used",
    ];

    for (const key of SHOULD_NOT_REDACT) {
        it(`does NOT redact ${key}`, () => {
            expect(isSecretKey(key)).toBe(false);
        });
    }
});

describe("sanitizeConfigValue — preserves benign config keys", () => {
    it("does not redact pin_key_files nested object", () => {
        const config = {
            dreamer: {
                pin_key_files: {
                    enabled: true,
                    token_budget: 10000,
                    min_reads: 4,
                },
            },
        };
        const sanitized = sanitizeConfigValue(config) as typeof config;
        expect(sanitized.dreamer.pin_key_files).toEqual({
            enabled: true,
            token_budget: 10000,
            min_reads: 4,
        });
    });

    it("still redacts embedding.api_key", () => {
        const config = {
            embedding: {
                provider: "openai-compatible",
                model: "text-embedding-3-small",
                endpoint: "http://localhost:1234/v1",
                api_key: "sk-supersecret-credential-value",
            },
        };
        const sanitized = sanitizeConfigValue(config) as Record<string, Record<string, unknown>>;
        expect(sanitized.embedding?.api_key).toBe("<REDACTED:api_key>");
        // Sibling fields under same parent must remain visible.
        expect(sanitized.embedding?.provider).toBe("openai-compatible");
        expect(sanitized.embedding?.model).toBe("text-embedding-3-small");
    });

    it("does not redact memory.injection_budget_tokens", () => {
        const config = {
            memory: {
                enabled: true,
                injection_budget_tokens: 4000,
                retrieval_count_promotion_threshold: 3,
            },
        };
        const sanitized = sanitizeConfigValue(config) as typeof config;
        expect(sanitized.memory.injection_budget_tokens).toBe(4000);
    });

    it("does not redact execute_threshold_tokens map", () => {
        const config = {
            execute_threshold_tokens: {
                default: 80000,
                "openai/gpt-5.5": 200000,
            },
        };
        const sanitized = sanitizeConfigValue(config) as typeof config;
        expect(sanitized.execute_threshold_tokens.default).toBe(80000);
        expect(sanitized.execute_threshold_tokens["openai/gpt-5.5"]).toBe(200000);
    });
});

describe("hasShareabilitySensitiveText", () => {
    it("treats personal paths and secrets as sensitive", () => {
        expect(hasShareabilitySensitiveText("Config lives under /Users/alice/private/repo")).toBe(
            true,
        );
        expect(hasShareabilitySensitiveText("Authorization: Bearer abcdefghijklmnop")).toBe(true);
    });

    it("allows non-sensitive project facts", () => {
        expect(hasShareabilitySensitiveText("Use Bun for test scripts in this repo.")).toBe(false);
    });
});

describe("sanitizeDiagnosticEndpoint", () => {
    it("strips query strings and userinfo from valid URLs", () => {
        const sanitized = sanitizeDiagnosticEndpoint(
            "https://user:pass@example.com/v1/embeddings?api_key=secret#frag",
        );
        expect(sanitized).toBe("https://example.com/v1/embeddings");
    });

    it("strips query strings and userinfo from invalid-scheme display values", () => {
        const sanitized = sanitizeDiagnosticEndpoint("ftp://user:pass@example.com/v1?token=secret");
        expect(sanitized).toBe("ftp://example.com/v1");
    });
});
