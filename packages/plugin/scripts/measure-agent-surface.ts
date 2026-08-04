#!/usr/bin/env bun
/**
 * Measure the agent-facing token surface: the Magic Context guidance section plus
 * every ctx_* tool description and parameter schema, per variant. Produces the
 * per-piece breakdown the #268 debloat discussion needs, and stays useful as a
 * regression check whenever agent-facing wording changes.
 *
 * Usage: bun packages/plugin/scripts/measure-agent-surface.ts
 */
import Tokenizer from "ai-tokenizer";
import * as claudeEncoding from "ai-tokenizer/encoding/claude";
import { buildMagicContextSection } from "../src/agents/magic-context-prompt";
import { CTX_EXPAND_DESCRIPTION } from "../src/tools/ctx-expand/constants";
import { CTX_MEMORY_DESCRIPTION } from "../src/tools/ctx-memory/constants";
import { CTX_NOTE_DESCRIPTION } from "../src/tools/ctx-note/constants";
import { CTX_REDUCE_DESCRIPTION } from "../src/tools/ctx-reduce/constants";
import { CTX_SEARCH_DESCRIPTION } from "../src/tools/ctx-search/constants";

type Row = { label: string; chars: number; tokens: number };

const tokenizer = new Tokenizer(claudeEncoding);

function measure(label: string, text: string): Row {
    return { label, chars: text.length, tokens: tokenizer.count(text) };
}

const rows: Row[] = [];

// Guidance section variants (primary session; memory on/off; reduce on/off).
// Signature: (agent, protectedTags, ctxReduceCallable, dreamerEnabled,
//             temporalAwareness, caveman, subagentMode, language, memoryEnabled)
for (const reduce of [true, false]) {
    for (const memory of [true, false]) {
        const section = buildMagicContextSection(null, 20, reduce, true, true, false, false, undefined, memory);
        rows.push(measure(`guidance reduce=${reduce} memory=${memory}`, section));
    }
}
rows.push(
    measure("guidance subagent minimal", buildMagicContextSection(null, 20, true, false, false, false, true)),
);

const descriptions: [string, string][] = [
    ["ctx_reduce", CTX_REDUCE_DESCRIPTION],
    ["ctx_expand", CTX_EXPAND_DESCRIPTION],
    ["ctx_note", CTX_NOTE_DESCRIPTION],
    ["ctx_memory", CTX_MEMORY_DESCRIPTION],
    ["ctx_search", CTX_SEARCH_DESCRIPTION],
];
for (const [name, desc] of descriptions) {
    rows.push(measure(`${name} description`, desc));
}

console.log("piece".padEnd(40), "chars".padStart(7), "tokens".padStart(7));
console.log("-".repeat(56));
for (const row of rows) {
    console.log(row.label.padEnd(40), String(row.chars).padStart(7), String(row.tokens).padStart(7));
}
const primary = rows.filter((r) => r.label === "guidance reduce=true memory=true" || r.label.endsWith("description"));
const total = primary.reduce((sum, r) => sum + r.tokens, 0);
console.log("-".repeat(56));
console.log("TOTAL primary surface (descriptions only)".padEnd(40), "".padStart(7), String(total).padStart(7));
console.log("\nNote: parameter schemas add provider-serialized overhead on top of");
console.log("descriptions; the reporter's 3.7k tool measurement includes schemas.");
