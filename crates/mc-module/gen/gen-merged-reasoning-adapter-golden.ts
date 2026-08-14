#!/usr/bin/env bun
import { writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { encodeOpenCodeMessagesToCk } from "../../../packages/plugin/src/hooks/magic-context/module-wire";

const here = dirname(fileURLToPath(import.meta.url));
const output = resolve(
	here,
	"../testdata/merged-reasoning-adapter-golden.json",
);

type RawPart = Record<string, unknown>;

function assistant(id: string, parts: RawPart[]): Record<string, unknown> {
	return {
		info: { id, role: "assistant" },
		parts,
	};
}

function rawMessages(
	name: string,
	reasoningPart: RawPart,
): Record<string, unknown>[] {
	return [
		assistant(`${name}-first`, [{ type: "text", text: "first answer" }]),
		assistant(`${name}-target`, [
			reasoningPart,
			{ type: "text", text: "target answer" },
		]),
		assistant(`${name}-newest`, [{ type: "text", text: "newest answer" }]),
	];
}

const fixtures = [
	{
		name: "reasoning",
		reasoningPart: {
			type: "reasoning",
			text: "reasoning trace",
			signature: "sig-reasoning",
		},
		expectStrip: true,
	},
	{
		name: "thinking",
		reasoningPart: {
			type: "thinking",
			thinking: "thinking trace",
			signature: "sig-thinking",
		},
		expectStrip: true,
	},
	{
		name: "redacted_thinking",
		reasoningPart: { type: "redacted_thinking", data: "redacted payload" },
		expectStrip: true,
	},
	{
		name: "reasoning_cache_control",
		reasoningPart: {
			type: "reasoning",
			text: "cached reasoning trace",
			signature: "sig-cache-control",
			cache_control: { type: "ephemeral" },
		},
		expectStrip: false,
	},
];

const cases = fixtures.map((fixture) => {
	const raw_messages = rawMessages(fixture.name, fixture.reasoningPart);
	return {
		name: fixture.name,
		target_mid: `${fixture.name}-target`,
		expect_strip: fixture.expectStrip,
		raw_messages,
		encoded_input: encodeOpenCodeMessagesToCk(raw_messages),
	};
});

writeFileSync(
	output,
	`${JSON.stringify({ generator_version: 1, cases }, null, 2)}\n`,
);
