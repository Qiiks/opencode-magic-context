/**
 * DG-1..5 reference generator.
 *
 * The reference side intentionally owns only canonical JSON and wire-visible fields. Rust
 * consumes the exact request fixtures in-process; neither side derives expected bytes from the
 * other. Keep this file dependency-free so regeneration works before the plugin is built.
 */
import { createHash } from "node:crypto";
import { dirname, join } from "node:path";

const generatorVersion = "dg-reference-v3";
const textMessage = (role: string, text: string, id?: string) => ({
  role,
  content: [{ kind: { type: "text", text } }],
  meta: id ? { harness_id: id } : {},
});
const syntheticTextMessage = (role: string, text: string, id: string) => ({
  ...textMessage(role, text, id),
  meta: { harness_id: id, synthetic: true },
});
const userTerminatedReference = (messages: readonly ReturnType<typeof textMessage>[]) => {
  const output = [...messages];
  const userIndex = output.findLastIndex((message) => message.role === "user");
  const trailing = output.slice(userIndex + 1);
  const contentless = trailing.every(
    (message) =>
      message.role === "assistant" &&
      message.content.every(
        (block) => block.kind.type === "text" && block.kind.text.trim().length === 0,
      ),
  );
  if (userIndex >= 0 && contentless) output.push(...output.splice(userIndex, 1));
  return output;
};
const scenarios = [
  {
    id: "DG-1-bust-veto",
    family: "postprocess-gates",
    input: { session_id: "dg-session", markers: ["bust", "veto"], messages: [textMessage("user", "stable input")] },
    output: { status: "ok", action: "passthrough", decision: "defer" },
  },
  {
    id: "DG-2-marker-representation",
    family: "marker-representation",
    input: { session_id: "dg-session", markers: ["<system-reminder>", "[dropped 2]"], messages: [textMessage("assistant", "kept tail")] },
    output: { status: "ok", action: "passthrough", decision: "replay" },
  },
  {
    id: "DG-3-escalation-band",
    family: "escalation-bands",
    input: { session_id: "dg-session", markers: ["band-275", "band-276"], messages: [textMessage("tool", "bounded output")] },
    output: { status: "ok", action: "passthrough", decision: "materialize" },
  },
  {
    id: "DG-4-contentless-assistant-tail",
    family: "user-terminated-tail",
    input: {
      session_id: "dg-assistant-tail",
      markers: ["assistant-prefill", "contentless"],
      messages: [textMessage("user", "retry prompt", "prompt"), textMessage("assistant", " \n", "dead-shell")],
    },
    output: { status: "ok", action: "passthrough", decision: "reanchor-user" },
  },
  {
    id: "DG-5-newest-synthetic-live-prompt",
    family: "newest-synthetic-user",
    input: {
      session_id: "dg-notice-triggered",
      markers: ["synthetic-user", "live-prompt"],
      messages: [
        textMessage("user", "original prompt", "prompt"),
        textMessage("assistant", "completed answer", "answer"),
        syntheticTextMessage(
          "user",
          "<system-reminder>[BACKGROUND BASH COMPLETED]</system-reminder>",
          "notice",
        ),
      ],
    },
    output: { status: "ok", action: "passthrough", decision: "preserve-live-prompt" },
  },
] as const;

const canonical = (value: unknown): string => JSON.stringify(value, null, 2) + "\n";
const inputHash = createHash("sha256").update(canonical(scenarios.map(({ id, family, input }) => ({ id, family, input })))).digest("hex");
const golden = {
  schema: 1,
  provenance: {
    generator: "crates/mc-module/gen/gen-differential-golden.ts",
    generator_version: generatorVersion,
    input_sha256: inputHash,
  },
  cases: scenarios.map(({ id, family, input, output }) => ({
    id,
    family,
    input,
    // The TS reference's canonical transform output is the wire-visible surface plus gates.
    expected: {
      ...output,
      wire:
        family === "user-terminated-tail"
          ? userTerminatedReference(input.messages)
          : input.messages,
    },
  })),
};

const outPath = join(dirname(import.meta.path), "../testdata/differential-golden.json");
await Bun.write(outPath, canonical(golden));
console.log(`wrote ${outPath} (${golden.cases.length} DG cases, input ${inputHash})`);
