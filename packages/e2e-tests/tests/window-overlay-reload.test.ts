/// <reference types="bun-types" />

import { describe, expect, it } from "bun:test";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { TestHarness } from "../src/harness";
import { PiTestHarness } from "../src/pi-harness";

const INITIAL_PRESSURE = 21.784593935169045;
const RELOADED_PRESSURE = 13.174536256323776;

function writeOverlay(path: string, enforcedWindow: number): void {
    const fact = {
        value: { kind: "stated", value: enforcedWindow },
        grade: "measured",
        units: "provider",
        boundary: "Observed",
        source_ref: "overlay-reload-e2e",
        observed_at: "2026-09-01T00:00:00Z",
    };
    writeFileSync(
        path,
        JSON.stringify({
            schema: "fusiform-window-overlay/v1",
            generated_at: "2026-09-01T00:00:00Z",
            minted_provider_ids: [],
            cells: [
                {
                    provider_id: "mock-anthropic",
                    model_id: "mock-sonnet",
                    facts: { "window.enforced": fact },
                },
                {
                    provider_id: "anthropic",
                    model_id: "claude-haiku-4-5",
                    facts: { "window.enforced": fact },
                },
            ],
        }),
    );
}

function persistedPressure(
    harness: TestHarness | PiTestHarness,
    sessionId: string,
): number | null {
    const row = harness
        .contextDb()
        .prepare("SELECT last_context_percentage FROM session_meta WHERE session_id = ?")
        .get(sessionId) as { last_context_percentage: number } | null;
    return row?.last_context_percentage ?? null;
}

async function settlePressure(): Promise<void> {
    // The provider response can settle just before the host event finishes its
    // SQLite write, so read after the same short persistence margin used by the
    // existing context-limit fixture.
    await Bun.sleep(300);
}

describe("same-path Fusiform overlay reload", () => {
    it("refreshes Pi on a hot extension reload and OpenCode on restart", async () => {
        const overlayRoot = mkdtempSync(join(tmpdir(), "mc-overlay-reload-"));
        const overlayPath = join(overlayRoot, "window-overlay.json");
        const config = {
            models: { window_overlay_path: overlayPath },
            execute_threshold_percentage: 80,
        };
        const usage = {
            input_tokens: 20_000,
            output_tokens: 50,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        };
        let opencode: TestHarness | null = null;
        let pi: PiTestHarness | null = null;
        let staleOpenCodePressure: number | null = null;
        let stalePiPressure: number | null = null;

        try {
            writeOverlay(overlayPath, 100_000);
            opencode = await TestHarness.create({
                magicContextConfig: config,
                modelContextLimit: 200_000,
            });
            opencode.mock.setDefault({ text: "ok", usage });
            const openCodeSession = await opencode.createSession();

            await opencode.sendPrompt(openCodeSession, "initial OpenCode overlay probe");
            await settlePressure();
            expect(persistedPressure(opencode, openCodeSession)).toBe(INITIAL_PRESSURE);

            writeOverlay(overlayPath, 160_000);
            await opencode.sendPrompt(openCodeSession, "same-path OpenCode rewrite probe");
            await settlePressure();
            staleOpenCodePressure = persistedPressure(opencode, openCodeSession);

            await opencode.restart();
            await opencode.sendPrompt(openCodeSession, "reloaded OpenCode overlay probe");
            await settlePressure();
            expect(persistedPressure(opencode, openCodeSession)).toBe(RELOADED_PRESSURE);
            await opencode.dispose();
            opencode = null;

            writeOverlay(overlayPath, 100_000);
            pi = await PiTestHarness.create({
                magicContextConfig: config,
                modelContextLimit: 200_000,
            });
            pi.mock.setDefault({ text: "ok", usage });

            const firstPiTurn = await pi.sendPrompt("initial Pi overlay probe", {
                timeoutMs: 60_000,
            });
            expect(firstPiTurn.sessionId).toBeTruthy();
            await settlePressure();
            expect(persistedPressure(pi, firstPiTurn.sessionId!)).toBe(INITIAL_PRESSURE);

            writeOverlay(overlayPath, 160_000);
            const stalePiTurn = await pi.sendPrompt("same-path Pi rewrite probe", {
                timeoutMs: 60_000,
            });
            expect(stalePiTurn.sessionId).toBe(firstPiTurn.sessionId);
            await settlePressure();
            stalePiPressure = persistedPressure(pi, stalePiTurn.sessionId!);

            await pi.reloadExtensions();
            const reloadedPiTurn = await pi.sendPrompt("hot-reloaded Pi overlay probe", {
                timeoutMs: 60_000,
            });
            expect(reloadedPiTurn.sessionId).toBe(firstPiTurn.sessionId);
            await settlePressure();
            expect(persistedPressure(pi, reloadedPiTurn.sessionId!)).toBe(RELOADED_PRESSURE);
            expect([staleOpenCodePressure, stalePiPressure]).toEqual([
                INITIAL_PRESSURE,
                INITIAL_PRESSURE,
            ]);
        } finally {
            await pi?.dispose();
            await opencode?.dispose();
            rmSync(overlayRoot, { recursive: true, force: true });
        }
    }, 300_000);
});
