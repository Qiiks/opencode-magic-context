/**
 * Shared support for the Rust-mode incident-corpus scenarios.
 *
 * Gating layers:
 *
 *  1. Prerequisite gating — `rustPrereqs` preflights the hermetic stack (cargo,
 *     the sibling subconscious workspace, a supported platform). A scenario
 *     `describe.skipIf(!rustPrereqs.ok)`s when the stack cannot run, printing the
 *     reason so CI logs never green-wash a skipped lane.
 *
 *  2. Fold-infrastructure gating — the Rust module runs its OWN historian, which
 *     drives an LLM through a separate `broca` runner module the hermetic stack
 *     does not spawn. Without it no compartment is ever published, so no fold
 *     (and no drop that only applies on a fold/execute cache-bust) can land.
 *     Fold-dependent scenarios (#6 fold-under-pressure, #7 ctx-reduce-roundtrip)
 *     are gated on `foldInfraEnabled()` and assert outcomes that activate once a
 *     hermetic broca runner is wired (MC_RUST_E2E_FOLD=1).
 *
 * The tail-mutation-readopt and park-self-heal scenarios are NOT gated: the P0
 * identity-drift / park-self-heal fix is merged into this branch's base, so they
 * assert the shipped mechanism (re-adopt, no permanent park) by default.
 */

import { RustTestHarness } from "./rust-harness";

export const rustPrereqs = RustTestHarness.detectPrereqs();

/** True once a hermetic broca LLM-runner module is wired so module-side folds can land. */
export function foldInfraEnabled(): boolean {
    return process.env.MC_RUST_E2E_FOLD === "1";
}

/**
 * Reason string for a fold-dependent scenario skip. The Rust module runs its own
 * historian, which drives an LLM through a separate `broca` runner module the
 * hermetic stack does not yet spawn; without it no compartment is ever published,
 * so no fold (or drop-on-fold) can land. Set MC_RUST_E2E_FOLD=1 once a hermetic
 * broca runner is added to the stack.
 */
export const FOLD_SKIP_REASON =
    "requires a hermetic broca LLM-runner module so the Rust module's historian can publish a compartment (fold); the current stack spawns only ck-subc + ck-mc (set MC_RUST_E2E_FOLD=1 once broca is wired)";

/** Enable the duplicate-ID regression only when the stack can produce the selection refresh needed to reproduce duplicate IDs. */
export function duplicateIdInfraEnabled(): boolean {
    return process.env.MC_RUST_E2E_DUPLICATE_IDS === "1";
}

/**
 * The hermetic stack has no broca runner, so it cannot complete the historian-backed
 * selection bust that consumes a queued ctx_reduce drop. Keep the assertion body
 * available for a provisioned runner instead of reporting a false green pass.
 */
export const DUPLICATE_ID_SKIP_REASON =
    "requires a broca-capable hermetic stack to reach the queued-drop selection bust (set MC_RUST_E2E_DUPLICATE_IDS=1 once that runner is provisioned)";

/**
 * Print a one-line skip notice. Call from a gated scenario's single `it` so the
 * reason is visible in the lane output (never a silent skip).
 */
export function printSkip(scenario: string, reason: string): void {
    console.log(`[rust-e2e] ${scenario} SKIPPED: ${reason}`);
}

/**
 * Drive a session to a steady SOFT+ defer state: one HARD first render followed
 * by `deferPasses` defers, each below any threshold. Returns once at least
 * `1 + deferPasses` rust passes are observed. Shared by scenarios that need an
 * established lineage before perturbing it.
 */
export async function driveToSteadyState(
    h: RustTestHarness,
    sessionId: string,
    deferPasses = 4,
): Promise<void> {
    for (let i = 1; i <= 1 + deferPasses; i += 1) {
        h.mock.setDefault({
            text: `steady assistant ${i}`,
            usage: {
                input_tokens: 2_000 * i,
                output_tokens: 20,
                cache_creation_input_tokens: 1_000,
            },
        });
        await h.sendPrompt(sessionId, `steady turn ${i}: ${h.ballast(400)}`);
    }
    await h.waitForRustPasses(1 + deferPasses);
}
