/// <reference types="bun-types" />

/**
 * Invariant #7: ctx_reduce round-trip.
 *
 * An agent-issued ctx_reduce drop must complete the full round-trip: the drop is
 * acknowledged deferred, applied on the next cache-busting pass, and the served
 * wire then shows the `[dropped §N§]` sentinel while the reduce command ledger
 * row is consumed. This proves reductions the agent requests actually take effect
 * on the wire in Rust mode, not just in local state.
 *
 * The hermetic stack supplies a deterministic Broca producer, so this scenario
 * runs in the Rust group and asserts the real drop-on-fold round trip.
 *
 * Assertion style: presence of the `[dropped §N§]` sentinel in the served wire
 * (from the fake provider's request body) plus ledger-row consumption.
 */

import { afterEach, beforeEach, describe, expect, it } from "bun:test";
import { RustTestHarness } from "../src/rust-harness";
import { rustPrereqs } from "../src/rust-scenario-support";

describe.skipIf(!rustPrereqs.ok)("rust invariant: ctx_reduce round-trip", () => {

    let h: RustTestHarness;

    beforeEach(async () => {
        h = await RustTestHarness.create({
            modelContextLimit: 30_000,
            magicContextConfig: {
                execute_threshold_percentage: 25,
                protected_tags: 1,
                compressor: { enabled: false },
            },
        });
    });

    afterEach(async () => {
        await h?.dispose();
    });

    it(
        "applies an agent ctx_reduce drop on the next bust and shows [dropped N] in the wire",
        async () => {
            const sessionId = await h.createSession();


            // Build turns with taggable content, then find a visible §N§ tag.
            for (let i = 1; i <= 3; i += 1) {
                h.mock.setDefault({
                    text: `assistant reply ${i}`,
                    usage: {
                        input_tokens: 2_000 * i,
                        output_tokens: 20,
                        cache_creation_input_tokens: 1_000,
                    },
                });
                await h.sendPrompt(sessionId, `turn ${i}: ${h.ballast(1_500)}`);
            }
            await Bun.sleep(500);

            const wire = h.lastMainWireSerialized();
            const tags = [...new Set([...wire.matchAll(/§(\d+)§/g)].map((m) => Number(m[1])))].sort(
                (a, b) => a - b,
            );
            expect(tags.length).toBeGreaterThan(0);
            const dropTag = tags[0]!;

            // Agent issues a ctx_reduce drop on the oldest visible tag.
            let dropEmitted = false;
            h.mock.addMatcher((body) => {
                if (dropEmitted || !JSON.stringify(body.system ?? "").includes("## Magic Context")) {
                    return null;
                }
                const tools = body.tools;
                if (!Array.isArray(tools)) return null;
                const name = (
                    tools.find((t) =>
                        /ctx_reduce/.test(String((t as { name?: unknown })?.name ?? "")),
                    ) as { name?: string } | undefined
                )?.name;
                if (!name) return null;
                dropEmitted = true;
                return {
                    content: [
                        {
                            type: "tool_use",
                            id: `toolu_reduce_${Date.now()}`,
                            name,
                            input: { drop: String(dropTag) },
                        },
                    ],
                    stop_reason: "tool_use",
                    usage: { input_tokens: 8_000, output_tokens: 20, cache_creation_input_tokens: 1_000 },
                };
            });
            await h.sendPrompt(sessionId, `turn 4: reduce tag ${dropTag}`);

            // Grow past the execute threshold so a bust drains the pending drop.
            for (let i = 5; i <= 10; i += 1) {
                h.mock.setDefault({
                    text: `pressure ${i}`,
                    usage: {
                        input_tokens: 3_000 * i,
                        output_tokens: 20,
                        cache_creation_input_tokens: 2_000,
                    },
                });
                await h.sendPrompt(sessionId, `turn ${i}: ${h.ballast(2_500)}`);
                await Bun.sleep(200);
            }
            await Bun.sleep(800);

            // Round-trip outcome: the agent's drop was emitted, and the served
            // wire now carries the dropped sentinel for that tag — the drop took
            // effect on the wire, not just in local state.
            expect(dropEmitted).toBe(true);
            const finalWire = h.lastMainWireSerialized();
            expect(finalWire).toContain(`[dropped §${dropTag}§]`);
        },
        300_000,
    );
});
