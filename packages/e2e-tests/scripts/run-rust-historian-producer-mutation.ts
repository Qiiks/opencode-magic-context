#!/usr/bin/env bun

import { readFileSync, writeFileSync } from "node:fs";
import { relative, resolve } from "node:path";

type CommandResult = { exit_status: number; output: string };

const e2eRoot = resolve(import.meta.dir, "..");
const repoRoot = resolve(e2eRoot, "../..");
const source = resolve(e2eRoot, "src/rust-runner/fake-broca.ts");
const oldText = "<p1>${title}</p1>";
const replacement = "<tier1>${title}</tier1>";
const decoder = new TextDecoder();

function runTest(expectBad: boolean): CommandResult {
    const env: Record<string, string> = { ...process.env, MC_E2E_MODE: "rust" };
    if (expectBad) env.MC_RUST_E2E_BROCA_EXPECT_BAD = "1";
    else delete env.MC_RUST_E2E_BROCA_EXPECT_BAD;
    const result = Bun.spawnSync({
        cmd: [
            "bun",
            "test",
            "--timeout",
            "600000",
            "--max-concurrency=1",
            "tests/rust-historian-producer.test.ts",
        ],
        cwd: e2eRoot,
        stdout: "pipe",
        stderr: "pipe",
        env,
    });
    return {
        exit_status: result.exitCode,
        output: `${decoder.decode(result.stdout)}${decoder.decode(result.stderr)}`,
    };
}

const before = readFileSync(source, "utf8");
if (before.split(oldText).length - 1 !== 1) {
    throw new Error("RUST_HISTORIAN_BAD_TIER: expected one tier mutation target");
}
const after = before.replace(oldText, replacement);
writeFileSync(source, after);
let observedFailure: CommandResult;
try {
    observedFailure = runTest(true);
} finally {
    writeFileSync(source, before);
}
const revertedRerun = runTest(false);
if (observedFailure.exit_status === 0) {
    throw new Error("RUST_HISTORIAN_BAD_TIER: mutation did not redden the validation assertion");
}
if (revertedRerun.exit_status !== 0) {
    throw new Error("RUST_HISTORIAN_BAD_TIER: reverted producer test did not pass");
}

const record = {
    drill: "RUST-HISTORIAN-PRODUCER",
    command: "MC_E2E_MODE=rust bun test --timeout 600000 --max-concurrency=1 tests/rust-historian-producer.test.ts",
    mutations: [
        {
            name: "RUST_HISTORIAN_BAD_TIER",
            applied_diff: {
                path: relative(repoRoot, source),
                before: oldText,
                after: replacement,
                changed: before !== after,
            },
            observed_failure: observedFailure,
            reverted_rerun: {
                ...revertedRerun,
                status: "pass",
            },
            adequacy_finding: null,
        },
    ],
};
writeFileSync(
    resolve(e2eRoot, "mutations/rust-historian-producer.json"),
    `${JSON.stringify(record, null, 2)}\n`,
);
console.log("wrote mutations/rust-historian-producer.json");
