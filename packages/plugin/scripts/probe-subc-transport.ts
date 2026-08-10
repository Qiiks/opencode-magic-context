#!/usr/bin/env bun

import { randomUUID } from "node:crypto";
import { homedir } from "node:os";
import { join, resolve } from "node:path";
import { monitorEventLoopDelay, performance } from "node:perf_hooks";
import {
    AdmissionClass,
    Priority,
    SubcClient,
    type RequestOptions,
} from "@cortexkit/subc-client";
import { SubcModuleTransport } from "../src/hooks/magic-context/module-transport";

type JsonRecord = Record<string, unknown>;

type Scenario = {
    name: string;
    body: JsonRecord;
    options?: RequestOptions;
};

type SampleSummary = {
    count: number;
    min_ms: number;
    p50_ms: number;
    p95_ms: number;
    max_ms: number;
    mean_ms: number;
};

const argv = process.argv.slice(2);

function option(name: string): string | undefined {
    const index = argv.indexOf(name);
    return index >= 0 ? argv[index + 1] : undefined;
}

function positiveInteger(name: string, fallback: number): number {
    const raw = option(name);
    if (raw === undefined) return fallback;
    const value = Number(raw);
    if (!Number.isSafeInteger(value) || value <= 0) {
        throw new Error(`${name} must be a positive integer`);
    }
    return value;
}

function round(value: number): number {
    return Number(value.toFixed(3));
}

function percentile(sorted: readonly number[], quantile: number): number {
    const index = Math.min(
        sorted.length - 1,
        Math.max(0, Math.ceil(sorted.length * quantile) - 1),
    );
    return sorted[index] ?? 0;
}

function summarize(samples: readonly number[]): SampleSummary {
    const sorted = [...samples].sort((left, right) => left - right);
    const total = sorted.reduce((sum, value) => sum + value, 0);
    return {
        count: sorted.length,
        min_ms: round(sorted[0] ?? 0),
        p50_ms: round(percentile(sorted, 0.5)),
        p95_ms: round(percentile(sorted, 0.95)),
        max_ms: round(sorted.at(-1) ?? 0),
        mean_ms: round(sorted.length === 0 ? 0 : total / sorted.length),
    };
}

function serializedBytes(value: unknown): number {
    return Buffer.byteLength(JSON.stringify(value));
}

function echoBody(serializedTargetBytes: number): JsonRecord {
    const body = { method: "echo", probe: "" };
    const fixedBytes = serializedBytes(body);
    if (serializedTargetBytes < fixedBytes) {
        throw new Error(`serialized echo target must be at least ${fixedBytes} bytes`);
    }
    body.probe = "x".repeat(serializedTargetBytes - fixedBytes);
    if (serializedBytes(body) !== serializedTargetBytes) {
        throw new Error("failed to construct an exact-size echo body");
    }
    return body;
}

function codecProxy(value: unknown, samples: number): {
    bytes: number;
    stringify: SampleSummary;
    parse: SampleSummary;
} {
    const serialized = JSON.stringify(value);
    const stringifySamples: number[] = [];
    const parseSamples: number[] = [];
    for (let index = 0; index < samples; index += 1) {
        let startedAt = performance.now();
        JSON.stringify(value);
        stringifySamples.push(performance.now() - startedAt);
        startedAt = performance.now();
        JSON.parse(serialized);
        parseSamples.push(performance.now() - startedAt);
    }
    return {
        bytes: Buffer.byteLength(serialized),
        stringify: summarize(stringifySamples),
        parse: summarize(parseSamples),
    };
}

if (argv.includes("--help")) {
    console.log(`usage: bun packages/plugin/scripts/probe-subc-transport.ts [options]

options:
  --connection-file <path>  daemon connection file
  --module-id <id>          module id (default: magic-context)
  --project-root <path>     route project root (default: cwd)
  --samples <n>             measured requests per scenario (default: 50)
  --warmup <n>              warmup requests per scenario (default: 5)
  --timeout-ms <n>          per-request deadline (default: 10000)
  --fifo-concurrency <n>    concurrent module-transport calls, max 16 (default: 8)`);
    process.exit(0);
}

const connectionFile = resolve(
    option("--connection-file") ??
        join(homedir(), ".local", "share", "cortexkit", "run", "subc-connection.json"),
);
const moduleId = option("--module-id") ?? "magic-context";
const projectRoot = resolve(option("--project-root") ?? process.cwd());
const samples = positiveInteger("--samples", 50);
if (samples < 30) throw new Error("--samples must be at least 30");
const warmup = positiveInteger("--warmup", 5);
const timeoutMs = positiveInteger("--timeout-ms", 10_000);
const fifoConcurrency = positiveInteger("--fifo-concurrency", 8);
if (fifoConcurrency > 16) throw new Error("--fifo-concurrency cannot exceed 16");
const session = `transport-probe-${process.pid}-${randomUUID()}`;
const smallBody = { method: "health", v: 1 };
const payloadSizes = [1, 4, 8, 16, 32].map((kibibytes) => kibibytes * 1024);
const productionRequestOptions: RequestOptions = {
    priority: Priority.Background,
    admissionClass: AdmissionClass.Normal,
    timeoutMs,
};
const scenarios: Scenario[] = [
    { name: "health_interactive", body: smallBody, options: { timeoutMs } },
    { name: "health_background_normal", body: smallBody, options: productionRequestOptions },
    ...payloadSizes.map((bytes) => ({
        name: `echo_${bytes / 1024}k_background_normal`,
        body: echoBody(bytes),
        options: productionRequestOptions,
    })),
];

const eventLoopDelay = monitorEventLoopDelay({ resolution: 1 });
eventLoopDelay.enable();
const connectStartedAt = performance.now();
const client = await SubcClient.connect({ connectionFile, handshakeTimeoutMs: timeoutMs });
const connectMs = performance.now() - connectStartedAt;
let routeOpenMs = 0;
let routeCloseMs = 0;
const results: Array<{
    name: string;
    request_bytes: number;
    response_bytes: number;
    round_trip: SampleSummary;
}> = [];

try {
    const routeOpenStartedAt = performance.now();
    const route = await client.routeOpen(
        { kind: "tool_provider", module_id: moduleId },
        { project_root: projectRoot, harness: "transport-probe", session },
    );
    routeOpenMs = performance.now() - routeOpenStartedAt;
    try {
        for (const scenario of scenarios) {
            for (let index = 0; index < warmup; index += 1) {
                await client.request(route, scenario.body, scenario.options);
            }
            const roundTrips: number[] = [];
            let response: unknown;
            for (let index = 0; index < samples; index += 1) {
                const startedAt = performance.now();
                response = await client.request(route, scenario.body, scenario.options);
                roundTrips.push(performance.now() - startedAt);
            }
            results.push({
                name: scenario.name,
                request_bytes: serializedBytes(scenario.body),
                response_bytes: serializedBytes(response),
                round_trip: summarize(roundTrips),
            });
        }
    } finally {
        const routeCloseStartedAt = performance.now();
        await client.closeRoute(route);
        routeCloseMs = performance.now() - routeCloseStartedAt;
    }
} finally {
    client.close();
}

type RuntimeTransport = {
    acquireCorrectnessLane: (
        sessionId: string,
        signal: AbortSignal | undefined,
        deadlineMs: number,
    ) => Promise<() => void>;
    invalidateConnection: () => void;
};

const transport = new SubcModuleTransport(connectionFile, moduleId, timeoutMs);
const runtimeTransport = transport as unknown as RuntimeTransport;
const originalAcquire = runtimeTransport.acquireCorrectnessLane.bind(transport);
const signalIndexes = new Map<AbortSignal, number>();
const laneWaits = Array.from<number>({ length: fifoConcurrency });
runtimeTransport.acquireCorrectnessLane = async (sessionId, signal, deadlineMs) => {
    const startedAt = performance.now();
    const release = await originalAcquire(sessionId, signal, deadlineMs);
    const index = signal ? signalIndexes.get(signal) : undefined;
    if (index !== undefined) laneWaits[index] = performance.now() - startedAt;
    return release;
};
const fifoSessions = Array.from(
    { length: fifoConcurrency },
    (_, index) => `${session}-fifo-${index}`,
);
const statusBody = (sessionId: string) => ({
    method: "session.status",
    v: 1,
    session_id: sessionId,
});
for (const sessionId of fifoSessions) {
    await transport.call({
        sessionId,
        projectRoot,
        method: "session.status",
        body: statusBody(sessionId),
    });
}
const fifoCallDurations = Array.from<number>({ length: fifoConcurrency });
const controllers = fifoSessions.map(() => new AbortController());
controllers.forEach((controller, index) => signalIndexes.set(controller.signal, index));
await Promise.all(
    fifoSessions.map(async (sessionId, index) => {
        const startedAt = performance.now();
        await transport.call({
            sessionId,
            projectRoot,
            method: "session.status",
            body: statusBody(sessionId),
            signal: controllers[index].signal,
        });
        fifoCallDurations[index] = performance.now() - startedAt;
    }),
);
const afterDequeueDurations = fifoCallDurations.map(
    (duration, index) => duration - laneWaits[index],
);
for (const sessionId of fifoSessions) transport.closeSession(sessionId);
runtimeTransport.invalidateConnection();
eventLoopDelay.disable();

const nanosecondsToMilliseconds = (value: number): number => round(value / 1_000_000);
console.log(
    JSON.stringify(
        {
            run: {
                timestamp: new Date().toISOString(),
                connection_file: connectionFile,
                module_id: moduleId,
                project_root: projectRoot,
                samples,
                warmup,
                sequential: true,
                route_reused: true,
            },
            setup: {
                connect_ms: round(connectMs),
                route_open_ms: round(routeOpenMs),
                route_close_ms: round(routeCloseMs),
            },
            scenarios: results,
            codec_proxy: Object.fromEntries(
                scenarios.map((scenario) => [scenario.name, codecProxy(scenario.body, samples)]),
            ),
            module_transport_fifo: {
                concurrency: fifoConcurrency,
                unrelated_session_status_calls: true,
                queue_wait: summarize(laneWaits),
                after_dequeue_to_response: summarize(afterDequeueDurations),
                total_call: summarize(fifoCallDurations),
            },
            event_loop_delay: {
                p50_ms: nanosecondsToMilliseconds(eventLoopDelay.percentile(50)),
                p95_ms: nanosecondsToMilliseconds(eventLoopDelay.percentile(95)),
                max_ms: nanosecondsToMilliseconds(eventLoopDelay.max),
            },
        },
        null,
        2,
    ),
);
