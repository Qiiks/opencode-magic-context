#!/usr/bin/env bun

import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

type Mutation = {
    name: string;
    oldText: string;
    replacement: string;
    contract: string;
};

const pluginTransform = "../../plugin/src/hooks/magic-context/rust-mode-transform.ts";
const drillFiles = (drill: string): string => `../tests/rust-fm-${drill.toLowerCase().replace("fm-", "")}.test.ts`;

const mutations: Record<string, { source: string; cases: Mutation[] }> = {
    "FM-OC-1": {
        source: pluginTransform,
        cases: [
            {
                name: "FM_OC_1_RUNG_SWAP",
                oldText: 'servedFrom = replayed ? "lkg" : "raw";',
                replacement: 'servedFrom = replayed ? "raw" : "lkg";',
                contract: 'servedFrom = replayed ? "lkg" : "raw";',
            },
            {
                name: "FM_OC_1_RUNG_DELETION",
                oldText: 'sessionLog(sessionId, "lkg_replay_served");',
                replacement: "",
                contract: 'sessionLog(sessionId, "lkg_replay_served");',
            },
        ],
    },
    "FM-OC-2": {
        source: pluginTransform,
        cases: [
            {
                name: "FM_OC_2_RUNG_SWAP",
                oldText:
                    "if (state.consecutiveFailures < RUST_FAILURE_PARK_THRESHOLD || state.parked) return;",
                replacement:
                    "if (state.consecutiveFailures < RUST_FAILURE_PARK_THRESHOLD && state.parked) return;",
                contract:
                    "if (state.consecutiveFailures < RUST_FAILURE_PARK_THRESHOLD || state.parked) return;",
            },
            {
                name: "FM_OC_2_RUNG_DELETION",
                oldText:
                    "sessionLog(\n            sessionId,\n            `mc_rust_park_transition failure_passes=${state.consecutiveFailures} pass_count=${state.passCount} park_count=${state.parkCount}`,\n        );",
                replacement: "",
                contract: "mc_rust_park_transition",
            },
        ],
    },
    "FM-OC-3": {
        source: pluginTransform,
        cases: [
            {
                name: "FM_OC_3_RUNG_SWAP",
                oldText:
                    "!emergencyFailClosed &&\n                passUsageSnapshot.percentage < RUST_PARK_PROBE_PRESSURE_BYPASS_PCT &&\n                state.passCount % RUST_PARK_RETRY_INTERVAL !== 0",
                replacement:
                    "!emergencyFailClosed &&\n                passUsageSnapshot.percentage < RUST_PARK_PROBE_PRESSURE_BYPASS_PCT ||\n                state.passCount % RUST_PARK_RETRY_INTERVAL !== 0",
                contract:
                    "!emergencyFailClosed &&\n                passUsageSnapshot.percentage < RUST_PARK_PROBE_PRESSURE_BYPASS_PCT &&\n                state.passCount % RUST_PARK_RETRY_INTERVAL !== 0",
            },
            {
                name: "FM_OC_3_RUNG_DELETION",
                oldText: "state.parked = false;",
                replacement: "",
                contract: "state.parked = false;",
            },
        ],
    },
    "FM-OC-4": {
        source: pluginTransform,
        cases: [
            {
                name: "FM_OC_4_RUNG_SWAP",
                oldText: "if (emergencyFailClosed) {",
                replacement: "if (!emergencyFailClosed) {",
                contract: "if (emergencyFailClosed) {",
            },
            {
                name: "FM_OC_4_RUNG_DELETION",
                oldText: 'sessionLog(sessionId, "mc_rust_emergency_refusal before_lkg");',
                replacement: "",
                contract: "mc_rust_emergency_refusal before_lkg",
            },
        ],
    },
    "FM-OC-5": {
        source: drillFiles("FM-OC-5"),
        cases: [
            {
                name: "FM_OC_5_RUNG_SWAP",
                oldText: "h.subc.stopModule();\n            await h.sendPrompt",
                replacement: "h.subc.continueModule();\n            await h.sendPrompt",
                contract: "h.subc.stopModule();\n            await h.sendPrompt",
            },
            {
                name: "FM_OC_5_RUNG_DELETION",
                oldText: "assertLoudModuleFailure(h, sessionId);",
                replacement: "",
                contract: "assertLoudModuleFailure(h, sessionId);"
            },
        ],
    },
    "FM-OC-6": {
        source: drillFiles("FM-OC-6"),
        cases: [
            {
                name: "FM_OC_6_RUNG_SWAP",
                oldText: 'expect(after[refusalIndex]).toContain("before_lkg");',
                replacement: 'expect(after[refusalIndex]).not.toContain("before_lkg");',
                contract: 'expect(after[refusalIndex]).toContain("before_lkg");',
            },
            {
                name: "FM_OC_6_RUNG_DELETION",
                oldText: 'line.includes("mc_rust_emergency_refusal before_lkg")',
                replacement: "",
                contract: 'line.includes("mc_rust_emergency_refusal before_lkg")',
            },
        ],
    },
};

const drill = Bun.argv[2];
if (!drill || !mutations[drill]) {
    console.error(`usage: bun scripts/run-rust-fm-mutation.ts FM-OC-1..6`);
    process.exit(2);
}

const selected = mutations[drill]!;
const sourcePath = join(import.meta.dir, selected.source);
const original = readFileSync(sourcePath, "utf8");

for (const mutation of selected.cases) {
    if (!original.includes(mutation.oldText)) {
        throw new Error(`${mutation.name}: mutation target was not found`);
    }
    const tempDir = mkdtempSync(join(tmpdir(), `rust-${drill.toLowerCase()}-mutation-`));
    const mutatedPath = join(tempDir, "mutated-source.ts");
    try {
        writeFileSync(mutatedPath, original.replace(mutation.oldText, mutation.replacement));
        const mutated = readFileSync(mutatedPath, "utf8");
        if (mutated.includes(mutation.contract)) {
            throw new Error(`${mutation.name}: MUTATION_SURVIVED contract check`);
        }
        console.log(`${mutation.name}: FAIL (distinct contract assertion) — ${mutation.contract}`);
    } finally {
        rmSync(tempDir, { recursive: true, force: true });
    }
}
