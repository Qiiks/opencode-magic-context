/// <reference types="bun-types" />

import { describe, expect, it } from "bun:test";
import { loadDefaultPiSessionApi } from "./pi-session-api";

/**
 * These tests exercise the DEFAULT resolution path against the actually
 * installed pi-coding-agent package. The Pi session-listing API drifted once
 * (`SessionManager.listSessions` never existed publicly) and the providers'
 * dependency-injected unit tests could not catch it — every test supplied its
 * own listSessions stub, so the broken default lookup only failed at runtime
 * inside the dreamer. This is the missing coverage: if pi-coding-agent renames
 * or removes the session APIs again, this fails in CI instead of silently
 * degrading retrospective/refresh-primers.
 */
describe("loadDefaultPiSessionApi", () => {
	it("resolves the session APIs from the installed pi-coding-agent", async () => {
		const api = await loadDefaultPiSessionApi();
		expect(typeof api.listSessions).toBe("function");
		expect(typeof api.loadEntriesFromFile).toBe("function");
	});

	it("parses JSONL session entries through the resolved loader", async () => {
		const api = await loadDefaultPiSessionApi();
		const { mkdtempSync, writeFileSync } = await import("node:fs");
		const { tmpdir } = await import("node:os");
		const { join } = await import("node:path");

		const dir = mkdtempSync(join(tmpdir(), "pi-session-api-test-"));
		const file = join(dir, "session.jsonl");
		const entry = {
			type: "message",
			id: "e1",
			message: {
				role: "user",
				timestamp: 123,
				content: [{ type: "text", text: "hello" }],
			},
		};
		writeFileSync(file, `${JSON.stringify(entry)}\n`);

		const entries = await api.loadEntriesFromFile(file);
		expect(Array.isArray(entries)).toBe(true);
		expect(entries.length).toBeGreaterThan(0);
	});
});
