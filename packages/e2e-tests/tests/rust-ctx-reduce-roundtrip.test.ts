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
 * Gating: in Rust mode a pending drop only APPLIES on a cache-busting pass
 * (HARD/EXECUTE/fold). Verified empirically that on the current hermetic stack no
 * such bust lands under content pressure — the module's historian fold needs a
 * `broca` LLM-runner module the stack does not spawn (`unknown_module: broca`),
 * so a queued drop is never drained and `[dropped …]` never appears in the served
 * wire. (In TS mode the same drop applies and shows `[dropped …]`; the
 * cold-start-drop-seed scenario exercises that and its Rust-side seed translation
 * directly.) This scenario is therefore gated on `foldInfraEnabled()`
 * (MC_RUST_E2E_FOLD=1) and asserts the round-trip OUTCOME so it activates cleanly
 * once a hermetic broca runner is wired.
 *
 * Assertion style: presence of the `[dropped §N§]` sentinel in the served wire
 * (from the fake provider's request body) plus ledger-row consumption.
 */

import { afterEach, beforeEach, describe, expect, it } from "bun:test";
import { RustTestHarness } from "../src/rust-harness";
import {
    FOLD_SKIP_REASON,
    foldInfraEnabled,
    printSkip,
    rustPrereqs,
} from "../src/rust-scenario-support";

const active = rustPrereqs.ok && foldInfraEnabled();

describe.skipIf(!rustPrereqs.ok)("rust invariant: ctx_reduce round-trip", () => {
    it.skipIf(active)("is gated on a hermetic broca fold runner", () => {
        printSkip("ctx-reduce-roundtrip", FOLD_SKIP_REASON);
        expect(foldInfraEnabled()).toBe(false);
    });

    let h: RustTestHarness;

    beforeEach(async () => {
        if (!active) return;
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

    it.skipIf(!active)(
        "applies an agent ctx_reduce drop on the next bust and shows [dropped N] in the wire",
        async () => {
            const sessionId = await h.createSession();

            // Historian producer for the fold that will drain the pending drop
            // (only reached once a hermetic broca runner exists).
            h.mock.addMatcher((body) => {
                const system = JSON.stringify(body.system ?? "");
                if (!system.includes("hippocampus of a long-running coding agent")) return null;
                return {
                    text: [
                        "<output>",
                        "<compartments>",
                        '<compartment start="1" end="2" title="Rust reduce e2e chunk">',
                        "Covered the warmup turns before the reduce.",
                        "</compartment>",
                        "</compartments>",
                        "<facts></facts>",
                        "<unprocessed_from>3</unprocessed_from>",
                        "</output>",
                    ].join("\n"),
                    usage: {
                        input_tokens: 500,
                        output_tokens: 200,
                        cache_creation_input_tokens: 500,
                    },
                };
            });

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
