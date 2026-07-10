import { createHash } from "node:crypto";

export interface TimingSample {
	stage: string;
	elapsedMs: number;
	extra?: string;
}

export interface PhaseTotals {
	entryBranch: number;
	tagIdentity: number;
	tagPrefix: number;
	targets: number;
	tokenCountingBackfill: number;
	stripsReplay: number;
	drops: number;
	boundaryTriggers: number;
	injection: number;
	dbIo: number;
	postTransform: number;
	total: number;
}

export interface DatabaseTiming {
	elapsedMs: number;
	operations: number;
	reads: number;
	writes: number;
}

export function createTimingCollector(): {
	observe(sample: TimingSample): void;
	reset(): void;
	samples(): TimingSample[];
} {
	let current: TimingSample[] = [];
	return {
		observe(sample) {
			current.push(sample);
		},
		reset() {
			current = [];
		},
		samples() {
			return current.slice();
		},
	};
}

export function createDatabaseTimer<T extends object>(
	database: T,
): {
	database: T;
	snapshot(): DatabaseTiming;
	reset(): void;
} {
	let elapsedMs = 0;
	let operations = 0;
	let reads = 0;
	let writes = 0;
	const statementCache = new WeakMap<object, object>();

	const time = <R>(kind: "read" | "write", operation: () => R): R => {
		const start = performance.now();
		try {
			return operation();
		} finally {
			elapsedMs += performance.now() - start;
			operations += 1;
			if (kind === "read") reads += 1;
			else writes += 1;
		}
	};

	const wrapStatement = (statement: object): object => {
		const cached = statementCache.get(statement);
		if (cached) return cached;
		const wrapped = new Proxy(statement, {
			get(target, property, _receiver) {
				const value = Reflect.get(target, property, target);
				if (typeof value !== "function") return value;
				if (property === "get" || property === "all" || property === "values") {
					return (...args: unknown[]) =>
						time("read", () => Reflect.apply(value, target, args));
				}
				if (property === "run") {
					return (...args: unknown[]) =>
						time("write", () => Reflect.apply(value, target, args));
				}
				return value.bind(target);
			},
		});
		statementCache.set(statement, wrapped);
		return wrapped;
	};

	const proxy = new Proxy(database, {
		get(target, property, _receiver) {
			const value = Reflect.get(target, property, target);
			if (typeof value !== "function") return value;
			if (property === "prepare" || property === "query") {
				return (...args: unknown[]) =>
					wrapStatement(Reflect.apply(value, target, args));
			}
			if (property === "exec") {
				return (...args: unknown[]) =>
					time("write", () => Reflect.apply(value, target, args));
			}
			return value.bind(target);
		},
	});

	return {
		database: proxy,
		snapshot: () => ({ elapsedMs, operations, reads, writes }),
		reset() {
			elapsedMs = 0;
			operations = 0;
			reads = 0;
			writes = 0;
		},
	};
}

export function summarizePhases(
	samples: readonly TimingSample[],
	database: DatabaseTiming,
): PhaseTotals {
	const stage = (name: string): number =>
		samples
			.filter((sample) => sample.stage === name)
			.reduce((sum, sample) => sum + sample.elapsedMs, 0);
	return {
		entryBranch: stage("entryParseAndBranchResolution"),
		tagIdentity: stage("fallbackIdentityAndAdoption") + stage("tag:identity"),
		tagPrefix: stage("tag:prefix"),
		targets: stage("tag:targets"),
		tokenCountingBackfill:
			stage("tag:tokenCounting") + stage("tokenAccounting"),
		stripsReplay:
			stage("applyFlushedStatuses") +
			stage("replayReasoningClearing") +
			stage("stripClearedReasoning"),
		drops: stage("applyPendingOperations") + stage("applyHeuristicCleanup"),
		boundaryTriggers: stage("boundaryTriggerChecks"),
		injection: stage("prepareCompartmentInjection"),
		dbIo: database.elapsedMs,
		postTransform: stage("postTransformPhase"),
		total: stage("total"),
	};
}

export function canonicalJson(value: unknown): string {
	return JSON.stringify(canonicalize(value));
}

export function canonicalHash(value: unknown): string {
	return createHash("sha256").update(canonicalJson(value)).digest("hex");
}

function canonicalize(value: unknown): unknown {
	if (Array.isArray(value)) return value.map(canonicalize);
	if (value === null || typeof value !== "object") return value;
	const record = value as Record<string, unknown>;
	const output: Record<string, unknown> = {};
	for (const key of Object.keys(record).sort()) {
		const item = record[key];
		if (item !== undefined) output[key] = canonicalize(item);
	}
	return output;
}
