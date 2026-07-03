#!/usr/bin/env bun
import { readFileSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const outPath = resolve(here, "pi-golden.json");
const check = process.argv.includes("--check");

const required = [
  "text_signature",
  "thinking_signature",
  "redacted_thinking",
  "image",
  "tool_call_split_pipe",
  "thought_signature",
  "tool_result",
  "tool_result_details",
  "custom_message",
  "compaction",
  "aborted_assistant",
  "response_id_mid",
  "timestamp_fallback_mid",
];

function fixture(): unknown {
  const existing = JSON.parse(readFileSync(outPath, "utf8"));
  assertCoverage(existing);
  return existing;
}

function assertCoverage(golden: any): void {
  const covered = new Set<string>(golden.coverage ?? []);
  const missing = required.filter((item) => !covered.has(item));
  if (missing.length > 0) {
    throw new Error(`Pi codec golden is missing coverage classes: ${missing.join(", ")}`);
  }
}

function stableStringify(value: any): string {
  if (value === null || typeof value !== "object") return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map(stableStringify).join(",")}]`;
  return `{${Object.keys(value)
    .sort()
    .map((key) => `${JSON.stringify(key)}:${stableStringify(value[key])}`)
    .join(",")}}`;
}

const bytes = `${JSON.stringify(fixture(), null, 2)}\n`;

if (check) {
  const existing = readFileSync(outPath, "utf8");
  if (stableStringify(JSON.parse(existing)) !== stableStringify(JSON.parse(bytes))) {
    throw new Error(`${outPath} is stale; run ${process.argv[1]}`);
  }
} else {
  writeFileSync(outPath, bytes);
}
