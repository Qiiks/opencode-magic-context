#!/usr/bin/env bun
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import type {
	ExtensionContext,
	SessionEntry,
} from "@earendil-works/pi-coding-agent";
import {
	buildAccumulationPasses,
	generateSyntheticFixture,
	loadFixture,
} from "./fixtures";
import {
	canonicalHash,
	canonicalJson,
	createDatabaseTimer,
	createTimingCollector,
	type DatabaseTiming,
	type PhaseTotals,
	summarizePhases,
} from "./instrumentation";

interface RunnerOptions {
	fixture?: string;
	messages: number;
	step: number;
	points?: number[];
	output?: string;
}

export interface PerfPassReport {
	pass: number;
	requestedMessages: number;
	inputMessages: number;
	outputMessages: number;
	outputBytes: number;
	outputHash: string;
	tagRows: number;
	tagRowsHash: string;
	phases: PhaseTotals;
	db: DatabaseTiming;
}

export interface PerfRunReport {
	schemaVersion: 1;
	fixture: string;
	fixtureBytes: number;
	sessionId: string;
	requestedStep: number;
	passes: PerfPassReport[];
}

async function main(): Promise<void> {
	const options = parseOptions(process.argv.slice(2));
	const fixture = options.fixture
		? loadFixture(options.fixture)
		: generateSyntheticFixture({ messages: options.messages });
	const points =
		options.points ??
		buildPoints(
			fixture.entries.filter((entry) => entry.type === "message").length,
			options.step,
		);
	const passes = buildAccumulationPasses(fixture, points);
	if (passes.length === 0)
		throw new Error(`No message passes found in ${fixture.name}`);

	const dataDir = mkdtempSync(join(tmpdir(), "mc-pi-perf-data-"));
	process.env.MAGIC_CONTEXT_TEST_DATA_DIR = dataDir;
	process.env.XDG_DATA_HOME = dataDir;
	process.env.NODE_ENV = "test";

	const [
		{ Database },
		{ initializeDatabase },
		{ runMigrations },
		{ setHarness },
	] = await Promise.all([
		import("@magic-context/core/shared/sqlite"),
		import("@magic-context/core/features/magic-context/storage-db"),
		import("@magic-context/core/features/magic-context/migrations"),
		import("@magic-context/core/shared/harness"),
	]);
	const [
		{ registerPiContextHandler, clearContextHandlerSession },
		{ setPiTransformTimingObserver },
	] = await Promise.all([
		import("../../../src/context-handler"),
		import("../../../src/context-perf-hooks"),
	]);

	setHarness("pi");
	const dbPath = join(dataDir, "context.db");
	const rawDb = new Database(dbPath);
	initializeDatabase(rawDb);
	runMigrations(rawDb);
	const dbTimer = createDatabaseTimer(rawDb);
	const timings = createTimingCollector();
	const restoreObserver = setPiTransformTimingObserver((sample) =>
		timings.observe(sample),
	);

	const handlers = new Map<string, (...args: unknown[]) => unknown>();
	const pi = {
		on(event: string, handler: (...args: unknown[]) => unknown) {
			handlers.set(event, handler);
		},
	};
	registerPiContextHandler(pi as never, {
		db: dbTimer.database as never,
		protectedTags: 20,
		scheduler: { executeThresholdPercentage: 95 },
		heuristics: { caveman: { enabled: false, minChars: 2_000 } },
		injection: {
			memoryEnabled: false,
			injectDocs: false,
			injectionBudgetTokens: 4_000,
			temporalAwareness: false,
		},
	});
	const handler = handlers.get("context");
	if (!handler)
		throw new Error(
			"registerPiContextHandler did not register a context handler",
		);

	const reports: PerfPassReport[] = [];
	try {
		for (let index = 0; index < passes.length; index += 1) {
			const pass = passes[index];
			if (!pass) continue;
			timings.reset();
			dbTimer.reset();
			const context = fakeContext(
				fixture.sessionId,
				fixture.cwd,
				pass.branchEntries,
			);
			const event = { messages: pass.messages };
			const inputMessageCount = event.messages.length;
			const result = (await handler(event, context)) as
				| { messages?: unknown[] }
				| undefined;
			const output = result?.messages ?? event.messages;
			const transformError = rawDb
				.prepare(
					"SELECT last_transform_error AS error FROM session_meta WHERE session_id = ?",
				)
				.get(fixture.sessionId) as { error?: string } | null;
			if (transformError?.error) {
				throw new Error(
					`Pi transform failed open on pass ${index + 1}: ${transformError.error}`,
				);
			}
			const dbSnapshot = dbTimer.snapshot();
			const tagRows = readTagRows(rawDb, fixture.sessionId);
			const outputCanonical = canonicalJson(output);
			reports.push({
				pass: index + 1,
				requestedMessages: pass.requestedMessages,
				inputMessages: inputMessageCount,
				outputMessages: output.length,
				outputBytes: Buffer.byteLength(outputCanonical, "utf8"),
				outputHash: canonicalHash(output),
				tagRows: tagRows.length,
				tagRowsHash: canonicalHash(tagRows),
				phases: summarizePhases(timings.samples(), dbSnapshot),
				db: dbSnapshot,
			});
		}
	} finally {
		restoreObserver();
		clearContextHandlerSession(fixture.sessionId);
		rawDb.close(false);
		rmSync(dataDir, { recursive: true, force: true });
	}

	const report: PerfRunReport = {
		schemaVersion: 1,
		fixture: fixture.name,
		fixtureBytes: fixture.sourceBytes,
		sessionId: fixture.sessionId,
		requestedStep: options.step,
		passes: reports,
	};
	const serialized = `${JSON.stringify(report, null, 2)}\n`;
	if (options.output) writeFileSync(resolve(options.output), serialized);
	else process.stdout.write(serialized);
}

function fakeContext(
	sessionId: string,
	cwd: string,
	branchEntries: readonly SessionEntry[],
): ExtensionContext {
	const byId = new Map(branchEntries.map((entry) => [entry.id, entry]));
	return {
		cwd,
		hasUI: false,
		signal: new AbortController().signal,
		ui: { notify: () => undefined },
		model: {
			provider: "anthropic",
			id: "claude-sonnet-4-5",
			contextWindow: 400_000,
		},
		sessionManager: {
			getSessionId: () => sessionId,
			getBranch: () => branchEntries as SessionEntry[],
			getEntry: (id: string) => byId.get(id),
		},
		getContextUsage: () => ({
			tokens: 1_000,
			percent: 0.25,
			contextWindow: 400_000,
		}),
	} as unknown as ExtensionContext;
}

function readTagRows(
	database: { prepare(sql: string): { all(...args: unknown[]): unknown[] } },
	sessionId: string,
): unknown[] {
	return database
		.prepare(
			`SELECT tag_number, message_id, type, status, drop_mode, tool_name,
                    input_byte_size, byte_size, reasoning_byte_size, caveman_depth,
                    tool_owner_message_id, entry_fingerprint, token_count,
                    input_token_count, reasoning_token_count, harness
             FROM tags
             WHERE session_id = ?
             ORDER BY tag_number ASC, id ASC`,
		)
		.all(sessionId);
}

function buildPoints(messageCount: number, step: number): number[] {
	const points: number[] = [];
	for (
		let point = Math.min(step, messageCount);
		point < messageCount;
		point += step
	) {
		points.push(point);
	}
	points.push(messageCount);
	return points;
}

function parseOptions(args: readonly string[]): RunnerOptions {
	const options: RunnerOptions = { messages: 1_000, step: 500 };
	for (let index = 0; index < args.length; index += 1) {
		const arg = args[index];
		const value = args[index + 1];
		if (arg === "--fixture" && value) {
			options.fixture = value;
			index += 1;
		} else if (arg === "--messages" && value) {
			options.messages = positiveInteger(value, arg);
			index += 1;
		} else if (arg === "--step" && value) {
			options.step = positiveInteger(value, arg);
			index += 1;
		} else if (arg === "--points" && value) {
			options.points = value
				.split(",")
				.map((item) => positiveInteger(item, arg));
			index += 1;
		} else if (arg === "--output" && value) {
			options.output = value;
			index += 1;
		} else {
			throw new Error(`Unknown or incomplete argument: ${arg ?? "<missing>"}`);
		}
	}
	return options;
}

function positiveInteger(value: string, option: string): number {
	const parsed = Number.parseInt(value, 10);
	if (!Number.isSafeInteger(parsed) || parsed <= 0) {
		throw new Error(`${option} requires a positive integer, got ${value}`);
	}
	return parsed;
}

await main();
