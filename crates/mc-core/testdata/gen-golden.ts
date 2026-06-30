/**
 * Generate the decay-curve golden fixture for the Rust port's differential test.
 *
 * Uses the production `decay-curve.ts` as the oracle: emits a grid of tier /
 * archive / rendered-tier cases plus budget-pressure cases, which the Rust
 * `decay_golden_matches_reference` test asserts against. Run after any change to
 * `decay-curve.ts` (or the Rust port) to re-baseline:
 *
 *   bun crates/mc-core/testdata/gen-golden.ts
 *
 * Writes decay-golden.json beside this file (committed as the Rust test fixture).
 */
import { mkdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import {
    computeBudgetPressure,
    computeBudgetPressureTwoPass,
    renderedTier,
    shouldArchive,
    tier,
} from "../../../packages/plugin/src/hooks/magic-context/decay-curve.ts";

const indices = [1, 2, 3, 5, 8, 13, 21, 34, 55, 89, 144, 200, 400, 1000];
const importances = [1, 10, 25, 40, 50, 60, 75, 90, 100];
const pressures = [0.1, 0.25, 0.5, 1.0, 1.5, 2.0, 4.0, 8.0];

const tierCases = [];
for (const index of indices) {
    for (const importance of importances) {
        for (const pressure of pressures) {
            tierCases.push({
                index,
                importance,
                pressure,
                tier: tier(index, importance, pressure),
                archived: shouldArchive(index, importance, pressure, 0),
                rendered: renderedTier(index, importance, pressure, 0),
            });
        }
    }
}

// Budget-pressure cases: a few compartment-pool shapes × budgets, incl. tight ones.
const pools = [
    Array.from({ length: 50 }, () => 50),
    Array.from({ length: 200 }, (_, i) => (i % 100) + 1),
    Array.from({ length: 500 }, (_, i) => [10, 50, 90][i % 3]),
];
const budgets = [60000, 20000, 8000, 2000, 500];
const pressureCases = [];
for (const importancesPool of pools) {
    for (const budget of budgets) {
        const comps = importancesPool.map((imp, i) => ({ index: i + 1, importance: imp }));
        pressureCases.push({
            importances: importancesPool,
            budget,
            one_pass: computeBudgetPressure(comps, budget),
            two_pass: computeBudgetPressureTwoPass(comps, budget),
        });
    }
}

const out = join(import.meta.dir, "decay-golden.json");
mkdirSync(dirname(out), { recursive: true });
writeFileSync(out, `${JSON.stringify({ tier_cases: tierCases, pressure_cases: pressureCases }, null, 2)}\n`);
console.log(`wrote ${tierCases.length} tier cases + ${pressureCases.length} pressure cases → ${out}`);
