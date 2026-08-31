#!/usr/bin/env bun

import { readFileSync, writeFileSync } from "node:fs";
import { relative, resolve } from "node:path";

const e2eRoot = resolve(import.meta.dir, "..");
const repoRoot = resolve(e2eRoot, "../..");
const sourcePath = resolve(repoRoot, "crates/mc-module/src/transform.rs");
const beforeTarget = `if is_sentinel_invisible_text_block(block)\n        || matches!(&block.kind, ck_wire::CkKind::Text { text } if is_dropped_placeholder_text(tag_stripped_text(text)))\n    {`;
const afterTarget = `if is_sentinel_invisible_text_block(block) {`;
const command = [
    "bun",
    "test",
    "--timeout",
    "700000",
    "--max-concurrency=1",
    "tests/rust-paired-session-replay.test.ts",
];

function run(): { exit_status: number; output: string } {
    const result = Bun.spawnSync({
        cmd: command,
        cwd: e2eRoot,
        stdout: "pipe",
        stderr: "pipe",
        env: process.env,
    });
    const decoder = new TextDecoder();
    return {
        exit_status: result.exitCode,
        output: `${decoder.decode(result.stdout)}${decoder.decode(result.stderr)}`,
    };
}

const before = readFileSync(sourcePath, "utf8");
if (before.split(beforeTarget).length - 1 !== 1) {
    throw new Error("paired replay mutation target must occur exactly once");
}
let observedFailure: ReturnType<typeof run>;
try {
    writeFileSync(sourcePath, before.replace(beforeTarget, afterTarget));
    observedFailure = run();
} finally {
    writeFileSync(sourcePath, before);
}
const revertedRerun = run();
const record = {
    drill: "PARITY-REPLAY-SIGNED-REASONING",
    command: command.join(" "),
    mutation: {
        name: "DROPPED_ASSISTANT_STEALS_REASONING_EXEMPTION",
        applied_diff: {
            path: relative(repoRoot, sourcePath),
            before: beforeTarget,
            after: afterTarget,
        },
        observed_failure: observedFailure,
        reverted_rerun: {
            ...revertedRerun,
            status: revertedRerun.exit_status === 0 ? "pass" : "fail",
        },
        adequacy_finding:
            observedFailure.exit_status === 0
                ? "mutation did not redden the replay drill"
                : null,
    },
};
writeFileSync(
    resolve(e2eRoot, "mutations/parity-replay.json"),
    `${JSON.stringify(record, null, 2)}\n`,
);
if (observedFailure.exit_status === 0 || revertedRerun.exit_status !== 0) process.exit(1);
